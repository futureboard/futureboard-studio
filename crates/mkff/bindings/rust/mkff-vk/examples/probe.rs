//! Diagnostic tool: lists Vulkan physical devices and reports whether
//! each has what `mkff-vk::VulkanImporter` needs (the four device
//! extensions plus the samplerYcbcrConversion feature). Run this on a
//! real GPU to sanity-check dma-buf import capability before wiring up
//! a decode pipeline; `VulkanImporter::new()` performs exactly this
//! same check internally and returns `NoSuitablePhysicalDevice` if
//! nothing qualifies.

use ash::vk;
use std::ffi::CStr;

fn main() {
    unsafe {
        let entry = ash::Entry::load().expect("load vulkan loader");
        let app_info = vk::ApplicationInfo::default().application_name(c"probe").api_version(vk::API_VERSION_1_1);
        let instance = entry
            .create_instance(&vk::InstanceCreateInfo::default().application_info(&app_info), None)
            .expect("create instance");

        let pds = instance.enumerate_physical_devices().expect("enumerate");
        println!("{} physical device(s)", pds.len());
        for pd in pds {
            let props = instance.get_physical_device_properties(pd);
            let name = CStr::from_ptr(props.device_name.as_ptr()).to_string_lossy();
            println!("- {name} (api {}.{}.{})", vk::api_version_major(props.api_version), vk::api_version_minor(props.api_version), vk::api_version_patch(props.api_version));

            let exts = instance.enumerate_device_extension_properties(pd).unwrap();
            for want in [
                vk::KHR_EXTERNAL_MEMORY_FD_NAME,
                vk::EXT_EXTERNAL_MEMORY_DMA_BUF_NAME,
                vk::EXT_IMAGE_DRM_FORMAT_MODIFIER_NAME,
                vk::EXT_QUEUE_FAMILY_FOREIGN_NAME,
            ] {
                let has = exts.iter().any(|e| CStr::from_ptr(e.extension_name.as_ptr()) == want);
                println!("    {want:?}: {has}");
            }

            let mut ycbcr = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default();
            let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut ycbcr);
            instance.get_physical_device_features2(pd, &mut features2);
            println!("    samplerYcbcrConversion: {}", ycbcr.sampler_ycbcr_conversion == vk::TRUE);
        }

        instance.destroy_instance(None);
    }
}
