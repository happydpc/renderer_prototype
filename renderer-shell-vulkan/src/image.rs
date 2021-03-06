use ash::vk;
use ash::prelude::VkResult;

use crate::device::VkDeviceContext;
use core::fmt;

#[derive(Copy, Clone, Debug)]
pub struct VkImageRaw {
    pub image: vk::Image,
    pub allocation: vk_mem::Allocation,
}

pub struct VkImage {
    pub device_context: VkDeviceContext,
    pub extent: vk::Extent3D,
    pub format: vk::Format,
    pub tiling: vk::ImageTiling,
    pub mip_level_count: u32,
    pub allocation_info: vk_mem::AllocationInfo,
    pub raw: Option<VkImageRaw>,
}

impl fmt::Debug for VkImage {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.debug_struct("VkImage")
            .field("raw", &self.raw)
            .field("extent", &self.extent)
            .finish()
    }
}

impl VkImage {
    pub fn new(
        device_context: &VkDeviceContext,
        memory_usage: vk_mem::MemoryUsage,
        image_usage: vk::ImageUsageFlags,
        extent: vk::Extent3D,
        format: vk::Format,
        tiling: vk::ImageTiling,
        samples: vk::SampleCountFlags,
        mip_level_count: u32,
        required_property_flags: vk::MemoryPropertyFlags,
    ) -> VkResult<Self> {
        let allocation_create_info = vk_mem::AllocationCreateInfo {
            usage: memory_usage,
            flags: vk_mem::AllocationCreateFlags::NONE,
            required_flags: required_property_flags,
            preferred_flags: vk::MemoryPropertyFlags::empty(),
            memory_type_bits: 0, // Do not exclude any memory types
            pool: None,
            user_data: None,
        };

        let image_create_info = vk::ImageCreateInfo::builder()
            .image_type(vk::ImageType::TYPE_2D)
            .extent(extent)
            .mip_levels(mip_level_count)
            .array_layers(1)
            .format(format)
            .tiling(tiling)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .usage(image_usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .samples(samples);

        //let allocator = device.allocator().clone();
        let (image, allocation, allocation_info) = device_context
            .allocator()
            .create_image(&image_create_info, &allocation_create_info)
            .map_err(|_| vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;

        let raw = VkImageRaw { image, allocation };

        Ok(VkImage {
            device_context: device_context.clone(),
            extent,
            format,
            tiling,
            mip_level_count,
            allocation_info,
            raw: Some(raw),
        })
    }

    pub fn image(&self) -> vk::Image {
        // Raw is only none if take_raw has not been called, and take_raw consumes the VkImage
        self.raw.unwrap().image
    }

    pub fn allocation(&self) -> vk_mem::Allocation {
        // Raw is only none if take_raw has not been called, and take_raw consumes the VkImage
        self.raw.unwrap().allocation
    }

    pub fn take_raw(mut self) -> Option<VkImageRaw> {
        let mut raw = None;
        std::mem::swap(&mut raw, &mut self.raw);
        raw
    }
}

impl Drop for VkImage {
    fn drop(&mut self) {
        log::trace!("destroying VkImage");

        if let Some(raw) = &self.raw {
            self.device_context
                .allocator()
                .destroy_image(raw.image, &raw.allocation)
                .unwrap();
        }

        log::trace!("destroyed VkImage");
    }
}
