use ash::vk;
use ash::prelude::VkResult;

use ash::version::DeviceV1_0;
use renderer_shell_vulkan::{VkDevice, VkQueueFamilyIndices, VkBuffer, VkDeviceContext};
use std::mem::ManuallyDrop;
use std::os::raw::c_void;
use ash::vk::MappedMemoryRange;

// Based on UploadHeap in cauldron
// (https://github.com/GPUOpen-LibrariesAndSDKs/Cauldron/blob/5acc12602c55e469cc1f9181967dbcb122f8e6c7/src/VK/base/UploadHeap.h)

struct VkUploader {
    device_context: VkDeviceContext,

    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,

    buffer: ManuallyDrop<VkBuffer>,
    mapped_memory: *mut u8,

    fence: vk::Fence,

    bytes_written_to_buffer: u64

    //buffer_begin: u32,
    //buffer_end: u32,
    //buffer_next_write_position: u32,
}

impl VkUploader {
    pub fn new(
        device: &VkDevice,
        size: u64
    ) -> VkResult<Self> {
        //
        // Command Buffers
        //
        let command_pool =
            Self::create_command_pool(device.device(), &device.queue_family_indices)?;

        let command_buffer = Self::create_command_buffer(device.device(), &command_pool)?;

        let buffer = ManuallyDrop::new(VkBuffer::new(
            &device.context,
            vk_mem::MemoryUsage::CpuOnly,
            vk::BufferUsageFlags::TRANSFER_SRC,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            size
        )?);

        let mapped_memory = unsafe {
            //TODO: Better way of handling allocator errors
            device.allocator().map_memory(
                &buffer.allocation
            ).map_err(|_| vk::Result::ERROR_MEMORY_MAP_FAILED)?
        };

        let fence = Self::create_fence(device.device())?;

        Self::begin_command_buffer(&device.device(), command_buffer);

        Ok(VkUploader {
            device_context: device.context.clone(),
            command_pool,
            command_buffer,
            buffer,
            mapped_memory,
            fence,
            bytes_written_to_buffer: 0
        })
    }

    fn create_command_pool(
        logical_device: &ash::Device,
        queue_family_indices: &VkQueueFamilyIndices,
    ) -> VkResult<vk::CommandPool> {
        //TODO: Consider a separate transfer queue
        log::info!(
            "Creating command pool with queue family index {}",
            queue_family_indices.graphics_queue_family_index
        );
        let pool_create_info = vk::CommandPoolCreateInfo::builder()
            .flags(
                vk::CommandPoolCreateFlags::TRANSIENT
                    | vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER,
            )
            .queue_family_index(queue_family_indices.graphics_queue_family_index);

        unsafe { logical_device.create_command_pool(&pool_create_info, None) }
    }

    fn create_command_buffer(
        logical_device: &ash::Device,
        command_pool: &vk::CommandPool,
    ) -> VkResult<vk::CommandBuffer> {
        let command_buffer_allocate_info = vk::CommandBufferAllocateInfo::builder()
            .command_buffer_count(1)
            .command_pool(*command_pool)
            .level(vk::CommandBufferLevel::PRIMARY);

        unsafe {
            Ok(logical_device.allocate_command_buffers(&command_buffer_allocate_info)?[0])
        }
    }

    fn create_fence(
        logical_device: &ash::Device,
    ) -> VkResult<vk::Fence> {
        let fence_create_info =
            vk::FenceCreateInfo::builder().flags(vk::FenceCreateFlags::empty());

        unsafe {
            Ok(logical_device.create_fence(&fence_create_info, None)?)
        }
    }

    fn begin_command_buffer(
        logical_device: &ash::Device,
        command_buffer: vk::CommandBuffer
    ) -> VkResult<()> {
        let command_buffer_begin_info = vk::CommandBufferBeginInfo::builder()
            .flags(vk::CommandBufferUsageFlags::empty());
        unsafe {
            logical_device.begin_command_buffer(command_buffer, &command_buffer_begin_info)
        }
    }

    pub fn push(&mut self) {
        //TODO: Push into the buffer
    }

    pub fn flush(&self) {
        // Not necessary, using coherent memory
        // unsafe {
        //     let mapped_memory_range = MappedMemoryRange::builder()
        //         .memory(self.buffer.buffer_memory)
        //         .size(self.bytes_written_to_buffer);
        //     self.device.flush_mapped_memory_ranges(&[*mapped_memory_range]);
        // }
    }

    pub fn flush_and_finish(&mut self) -> VkResult<()> {
        self.flush();

        unsafe {
            self.device_context.device().end_command_buffer(self.command_buffer)?;
        }

        //TODO: Submit and wait for fence

        Self::begin_command_buffer(&self.device_context.device(), self.command_buffer)

    }
}

impl Drop for VkUploader {
    fn drop(&mut self) {
        log::debug!("destroying VkUploader");

        unsafe {
            self.device_context.allocator().unmap_memory(&self.buffer.allocation);
            ManuallyDrop::drop(&mut self.buffer);
            self.device_context.device().destroy_command_pool(self.command_pool, None);
            self.device_context.device().destroy_fence(self.fence, None);
        }

        log::debug!("destroyed VkSpriteRenderPass");
    }
}
