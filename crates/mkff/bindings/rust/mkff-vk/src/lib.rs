//! Zero-copy import of MKFF decoded frames (`mkff::LinuxDmaBuf`) into a
//! Vulkan `VkImage`, via `VK_EXT_external_memory_dma_buf` +
//! `VK_EXT_image_drm_format_modifier`. No pixel data is copied or read
//! back to the CPU at any point: the dma-buf file descriptor exported
//! by the decoder is imported directly as the image's backing memory.
//!
//! This is deliberately narrow in scope: it supports the single-object
//! NV12 layout that `mkff`'s Linux platform module actually produces
//! (`VA_EXPORT_SURFACE_COMPOSED_LAYERS`), not the general case of
//! disjoint multi-object planar dma-bufs. That's a real limitation, not
//! a stub — extending it to the disjoint case is mechanical (bind each
//! plane separately via `VkBindImagePlaneMemoryInfo`) but out of scope
//! for this first pass.

use std::ffi::CStr;
use std::fmt;
use std::os::fd::AsRawFd;

use ash::vk;

/// DRM_FORMAT_NV12 (see `drm_fourcc.h`). Same fourcc encoding VA-API
/// uses for `VA_FOURCC_NV12`; hardcoded here rather than pulling in a
/// libdrm-sys crate for one constant.
const DRM_FORMAT_NV12: u32 = 0x3231_564E;

#[derive(Debug)]
pub enum VkImportError {
    Loading(ash::LoadingError),
    Vulkan(vk::Result),
    NoSuitablePhysicalDevice,
    UnsupportedFormat(u32),
    UnsupportedLayout(&'static str),
    NoCompatibleMemoryType,
}

impl fmt::Display for VkImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VkImportError::Loading(e) => write!(f, "failed to load the Vulkan loader: {e}"),
            VkImportError::Vulkan(e) => write!(f, "Vulkan error: {e}"),
            VkImportError::NoSuitablePhysicalDevice => write!(
                f,
                "no Vulkan physical device supports dma-buf import (VK_KHR_external_memory_fd + \
                 VK_EXT_external_memory_dma_buf + VK_EXT_image_drm_format_modifier + samplerYcbcrConversion)"
            ),
            VkImportError::UnsupportedFormat(fourcc) => write!(f, "unsupported DRM fourcc 0x{fourcc:08x}"),
            VkImportError::UnsupportedLayout(msg) => write!(f, "unsupported dma-buf layout: {msg}"),
            VkImportError::NoCompatibleMemoryType => write!(f, "no compatible Vulkan memory type for the imported dma-buf"),
        }
    }
}

impl std::error::Error for VkImportError {}

impl From<ash::LoadingError> for VkImportError {
    fn from(e: ash::LoadingError) -> Self {
        VkImportError::Loading(e)
    }
}

impl From<vk::Result> for VkImportError {
    fn from(e: vk::Result) -> Self {
        VkImportError::Vulkan(e)
    }
}

fn vk_format_for_drm_fourcc(fourcc: u32) -> Option<vk::Format> {
    match fourcc {
        DRM_FORMAT_NV12 => Some(vk::Format::G8_B8R8_2PLANE_420_UNORM),
        _ => None,
    }
}

const REQUIRED_DEVICE_EXTENSIONS: [&CStr; 4] = [
    vk::KHR_EXTERNAL_MEMORY_FD_NAME,
    vk::EXT_EXTERNAL_MEMORY_DMA_BUF_NAME,
    vk::EXT_IMAGE_DRM_FORMAT_MODIFIER_NAME,
    vk::EXT_QUEUE_FAMILY_FOREIGN_NAME,
];

/// Owns a `VkInstance` + `VkDevice` selected for dma-buf import
/// capability. Not `Send`/`Sync`: Vulkan objects generally require
/// external synchronization, and this crate does none.
pub struct VulkanImporter {
    _entry: ash::Entry,
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    ext_memory_fd: ash::khr::external_memory_fd::Device,
}

impl VulkanImporter {
    pub fn new() -> Result<Self, VkImportError> {
        let entry = unsafe { ash::Entry::load()? };

        let app_info = vk::ApplicationInfo::default().application_name(c"mkff-vk").api_version(vk::API_VERSION_1_1);
        let instance_create_info = vk::InstanceCreateInfo::default().application_info(&app_info);
        let instance = unsafe { entry.create_instance(&instance_create_info, None)? };

        match Self::select_and_create_device(&entry, &instance) {
            Ok((physical_device, device)) => {
                let ext_memory_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
                Ok(VulkanImporter {
                    _entry: entry,
                    instance,
                    physical_device,
                    device,
                    ext_memory_fd,
                })
            }
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    fn select_and_create_device(_entry: &ash::Entry, instance: &ash::Instance) -> Result<(vk::PhysicalDevice, ash::Device), VkImportError> {
        let physical_devices = unsafe { instance.enumerate_physical_devices()? };

        for pd in physical_devices {
            let supported_extensions = match unsafe { instance.enumerate_device_extension_properties(pd) } {
                Ok(exts) => exts,
                Err(_) => continue,
            };
            let has_all_required = REQUIRED_DEVICE_EXTENSIONS.iter().all(|required| {
                supported_extensions
                    .iter()
                    .any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == *required)
            });
            if !has_all_required {
                continue;
            }

            let mut ycbcr_query = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default();
            let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut ycbcr_query);
            unsafe { instance.get_physical_device_features2(pd, &mut features2) };
            if ycbcr_query.sampler_ycbcr_conversion == vk::FALSE {
                continue;
            }

            let queue_families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
            if queue_families.is_empty() {
                continue;
            }
            let queue_family_index = 0u32;

            let queue_priorities = [1.0f32];
            let queue_create_infos = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family_index)
                .queue_priorities(&queue_priorities)];

            let ext_name_ptrs: Vec<*const std::os::raw::c_char> = REQUIRED_DEVICE_EXTENSIONS.iter().map(|n| n.as_ptr()).collect();

            let mut enable_ycbcr = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default().sampler_ycbcr_conversion(true);
            let device_create_info = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_create_infos)
                .enabled_extension_names(&ext_name_ptrs)
                .push_next(&mut enable_ycbcr);

            match unsafe { instance.create_device(pd, &device_create_info, None) } {
                Ok(device) => return Ok((pd, device)),
                Err(_) => continue,
            }
        }

        Err(VkImportError::NoSuitablePhysicalDevice)
    }

    fn find_memory_type(&self, type_bits: u32, required_properties: vk::MemoryPropertyFlags) -> Option<u32> {
        let mem_properties = unsafe { self.instance.get_physical_device_memory_properties(self.physical_device) };
        (0..mem_properties.memory_type_count).find(|&i| {
            let type_supported = (type_bits & (1 << i)) != 0;
            let has_properties = mem_properties.memory_types[i as usize].property_flags.contains(required_properties);
            type_supported && has_properties
        })
    }

    /// Imports the dma-buf surface backing a decoded frame as a
    /// `VkImage`. Consumes `dmabuf`: its file descriptor either
    /// transfers ownership to the Vulkan implementation (on success) or
    /// is closed as `dmabuf` drops (on failure) — never both, never
    /// neither.
    pub fn import(&self, mut dmabuf: mkff::LinuxDmaBuf) -> Result<ImportedImage<'_>, VkImportError> {
        if dmabuf.objects.len() != 1 {
            return Err(VkImportError::UnsupportedLayout(
                "only single-dma-buf-object surfaces are supported (the exported NV12 surface should be one object per VA_EXPORT_SURFACE_COMPOSED_LAYERS)",
            ));
        }
        let format = vk_format_for_drm_fourcc(dmabuf.drm_fourcc).ok_or(VkImportError::UnsupportedFormat(dmabuf.drm_fourcc))?;

        let plane_layouts: Vec<vk::SubresourceLayout> = dmabuf
            .planes
            .iter()
            .map(|p| vk::SubresourceLayout {
                offset: p.offset as u64,
                size: 0, // ignored by drivers for VkImageDrmFormatModifierExplicitCreateInfoEXT
                row_pitch: p.pitch as u64,
                array_pitch: 0,
                depth_pitch: 0,
            })
            .collect();

        let modifier = dmabuf.objects[0].modifier;

        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&plane_layouts);
        let mut external_info = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let image_create_info = vk::ImageCreateInfo::default()
            .push_next(&mut external_info)
            .push_next(&mut modifier_info)
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: dmabuf.width,
                height: dmabuf.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        let image = unsafe { self.device.create_image(&image_create_info, None)? };

        let object = dmabuf.objects.remove(0);
        let raw_fd = object.fd.as_raw_fd();

        let result = self.import_memory_for_image(image, raw_fd, &object, &modifier);
        match result {
            Ok(memory) => {
                // Ownership of the fd transferred to Vulkan on a
                // successful import: forget the OwnedFd rather than
                // letting it close(2) the fd Vulkan now owns.
                std::mem::forget(object.fd);
                Ok(ImportedImage {
                    importer: self,
                    image,
                    memory,
                    format,
                    width: dmabuf.width,
                    height: dmabuf.height,
                })
            }
            Err(e) => {
                unsafe { self.device.destroy_image(image, None) };
                // `object.fd` drops here normally (closing the fd):
                // import did not take ownership on failure.
                Err(e)
            }
        }
    }

    fn import_memory_for_image(&self, image: vk::Image, raw_fd: i32, object: &mkff::LinuxDmaBufObject, _modifier: &u64) -> Result<vk::DeviceMemory, VkImportError> {
        let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
        unsafe {
            self.ext_memory_fd
                .get_memory_fd_properties(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT, raw_fd, &mut fd_properties)
        }?;

        let mem_requirements = unsafe { self.device.get_image_memory_requirements(image) };
        let memory_type_bits = mem_requirements.memory_type_bits & fd_properties.memory_type_bits;
        let memory_type_index = self
            .find_memory_type(memory_type_bits, vk::MemoryPropertyFlags::empty())
            .ok_or(VkImportError::NoCompatibleMemoryType)?;

        let _ = object.size; // available for validation; not required by the driver here

        let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(raw_fd);

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut dedicated_info)
            .push_next(&mut import_info);

        let memory = unsafe { self.device.allocate_memory(&alloc_info, None)? };

        if let Err(e) = unsafe { self.device.bind_image_memory(image, memory, 0) } {
            unsafe { self.device.free_memory(memory, None) };
            return Err(e.into());
        }

        Ok(memory)
    }
}

impl Drop for VulkanImporter {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// An imported `VkImage` backed directly by a decoded frame's dma-buf
/// memory — no copy. Destroyed (image + memory freed) on drop.
pub struct ImportedImage<'a> {
    importer: &'a VulkanImporter,
    pub image: vk::Image,
    memory: vk::DeviceMemory,
    pub format: vk::Format,
    pub width: u32,
    pub height: u32,
}

impl Drop for ImportedImage<'_> {
    fn drop(&mut self) {
        unsafe {
            self.importer.device.destroy_image(self.image, None);
            self.importer.device.free_memory(self.memory, None);
        }
    }
}
