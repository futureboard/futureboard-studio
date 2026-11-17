use std::fs::File;
use std::os::fd::OwnedFd;

use mkff::{LinuxDmaBuf, LinuxDmaBufObject, LinuxDmaBufPlane};
use mkff_vk::{VkImportError, VulkanImporter};

fn dummy_fd() -> OwnedFd {
    // Any valid fd works for the tests below: they exercise validation
    // that happens before the fd is ever handed to Vulkan (unsupported
    // format / unsupported multi-object layout), so it never needs to
    // actually be a dma-buf.
    File::open("/dev/null").expect("/dev/null should always be openable").into()
}

#[test]
fn importer_creation_does_not_panic() {
    // On a real GPU with VK_EXT_image_drm_format_modifier support this
    // succeeds. In this sandbox (software Vulkan via lavapipe) it's
    // expected to fail with NoSuitablePhysicalDevice, since lavapipe
    // doesn't implement that extension — a driver/hardware limitation,
    // not a bug in VulkanImporter. Either outcome is acceptable here;
    // a panic is not.
    match VulkanImporter::new() {
        Ok(_importer) => {}
        Err(e) => {
            let _ = e.to_string();
        }
    }
}

#[test]
fn import_rejects_multi_object_layout() {
    let Ok(importer) = VulkanImporter::new() else {
        return; // no compatible Vulkan device in this environment
    };

    let dmabuf = LinuxDmaBuf {
        drm_fourcc: 0x3231_564E, // DRM_FORMAT_NV12
        width: 64,
        height: 64,
        objects: vec![
            LinuxDmaBufObject { fd: dummy_fd(), size: 4096, modifier: 0 },
            LinuxDmaBufObject { fd: dummy_fd(), size: 4096, modifier: 0 },
        ],
        planes: vec![
            LinuxDmaBufPlane { object_index: 0, offset: 0, pitch: 64 },
            LinuxDmaBufPlane { object_index: 1, offset: 0, pitch: 64 },
        ],
    };

    let result = importer.import(dmabuf);
    assert!(matches!(result, Err(VkImportError::UnsupportedLayout(_))));
}

#[test]
fn import_rejects_unsupported_fourcc() {
    let Ok(importer) = VulkanImporter::new() else {
        return; // no compatible Vulkan device in this environment
    };

    let dmabuf = LinuxDmaBuf {
        drm_fourcc: 0xFFFF_FFFF, // not a format mkff-vk knows about
        width: 64,
        height: 64,
        objects: vec![LinuxDmaBufObject { fd: dummy_fd(), size: 4096, modifier: 0 }],
        planes: vec![LinuxDmaBufPlane { object_index: 0, offset: 0, pitch: 64 }],
    };

    let result = importer.import(dmabuf);
    assert!(matches!(result, Err(VkImportError::UnsupportedFormat(0xFFFF_FFFF))));
}
