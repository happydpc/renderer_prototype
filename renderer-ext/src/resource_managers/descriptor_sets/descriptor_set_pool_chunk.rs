use ash::vk;
use ash::version::DeviceV1_0;
use super::{
    DescriptorLayoutBufferSet, DescriptorSetPoolRequiredBufferInfo, MAX_DESCRIPTORS_PER_POOL,
    MAX_FRAMES_IN_FLIGHT_PLUS_1, RegisteredDescriptorSet, DescriptorSetWriteSet,
    FrameInFlightIndex, MAX_FRAMES_IN_FLIGHT, DescriptorSetWriteBuffer, DescriptorSetElementKey,
};
use std::collections::VecDeque;
use renderer_shell_vulkan::{VkDeviceContext, VkDescriptorPoolAllocator, VkResourceDropSink, VkBuffer};
use ash::prelude::VkResult;
use arrayvec::ArrayVec;
use std::mem::ManuallyDrop;
use renderer_base::slab::RawSlabKey;
use fnv::FnvHashMap;
use crate::pipeline_description as dsc;

// A write to the descriptors within a single descriptor set that has been scheduled (i.e. will occur
// over the next MAX_FRAMES_IN_FLIGHT_PLUS_1 frames
#[derive(Debug)]
struct PendingDescriptorSetWriteSet {
    slab_key: RawSlabKey<RegisteredDescriptorSet>,
    write_set: DescriptorSetWriteSet,
    live_until_frame: FrameInFlightIndex,
}

// A write to the buffers within a single descriptor set that has been scheduled (i.e. will occur
// over the next MAX_FRAMES_IN_FLIGHT_PLUS_1 frames
#[derive(Debug)]
struct PendingDescriptorSetWriteBuffer {
    slab_key: RawSlabKey<RegisteredDescriptorSet>,
    write_buffer: DescriptorSetWriteBuffer,
    live_until_frame: FrameInFlightIndex,
}

//
// A single chunk within a pool. This allows us to create MAX_DESCRIPTORS_PER_POOL * MAX_FRAMES_IN_FLIGHT_PLUS_1
// descriptors for a single descriptor set layout
//
pub(super) struct RegisteredDescriptorSetPoolChunk {
    // We only need the layout for logging
    descriptor_set_layout: vk::DescriptorSetLayout,

    // The pool holding all descriptors in this chunk
    pool: vk::DescriptorPool,

    // The MAX_DESCRIPTORS_PER_POOL descriptors
    descriptor_sets: Vec<Vec<vk::DescriptorSet>>,

    // The buffers that back the descriptor sets
    buffers: DescriptorLayoutBufferSet,

    // The writes that have been scheduled to occur over the next MAX_FRAMES_IN_FLIGHT_PLUS_1 frames. This
    // ensures that each frame's descriptor sets/buffers are appropriately updated
    pending_set_writes: VecDeque<PendingDescriptorSetWriteSet>,
    pending_buffer_writes: VecDeque<PendingDescriptorSetWriteBuffer>,
}

impl RegisteredDescriptorSetPoolChunk {
    pub(super) fn new(
        device_context: &VkDeviceContext,
        buffer_info: &[DescriptorSetPoolRequiredBufferInfo],
        descriptor_set_layout: vk::DescriptorSetLayout,
        allocator: &mut VkDescriptorPoolAllocator,
    ) -> VkResult<Self> {
        let pool = allocator.allocate_pool(device_context.device())?;

        // This structure describes how the descriptor sets will be allocated.
        let descriptor_set_layouts = [descriptor_set_layout; MAX_DESCRIPTORS_PER_POOL as usize];

        // We need to allocate the full set once per frame in flight, plus one frame not-in-flight
        // that we can modify
        let mut descriptor_sets = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT_PLUS_1);
        for _ in 0..MAX_FRAMES_IN_FLIGHT_PLUS_1 {
            let set_create_info = vk::DescriptorSetAllocateInfo::builder()
                .descriptor_pool(pool)
                .set_layouts(&descriptor_set_layouts);

            let descriptor_sets_for_frame = unsafe {
                device_context
                    .device()
                    .allocate_descriptor_sets(&*set_create_info)?
            };
            descriptor_sets.push(descriptor_sets_for_frame);
        }

        // Now allocate all the buffers that act as backing-stores for descriptor sets
        let buffers = DescriptorLayoutBufferSet::new(device_context, buffer_info)?;

        // There is some trickiness here, vk::WriteDescriptorSet will hold a pointer to vk::DescriptorBufferInfos
        // that have been pushed into `write_descriptor_buffer_infos`. We don't want to use a Vec
        // since it can realloc and invalidate the pointers.
        const DESCRIPTOR_COUNT: usize =
            (MAX_FRAMES_IN_FLIGHT_PLUS_1) * MAX_DESCRIPTORS_PER_POOL as usize;
        let mut write_descriptor_buffer_infos: ArrayVec<[_; DESCRIPTOR_COUNT]> = ArrayVec::new();
        let mut descriptor_writes = Vec::new();

        // For every binding/buffer set
        for (binding_key, binding_buffers) in &buffers.buffer_sets {
            // For every per-frame buffer
            for (binding_buffer_for_frame, binding_descriptors_for_frame) in
                binding_buffers.buffers.iter().zip(&descriptor_sets)
            {
                // For every descriptor
                let mut offset = 0;
                for descriptor_set in binding_descriptors_for_frame {
                    let buffer_info = [vk::DescriptorBufferInfo::builder()
                        .buffer(binding_buffer_for_frame.buffer())
                        .range(binding_buffers.buffer_info.per_descriptor_size as u64)
                        .offset(offset)
                        .build()];

                    // The array of buffer infos has to persist until all WriteDescriptorSet are
                    // built and written
                    write_descriptor_buffer_infos.push(buffer_info);

                    let descriptor_set_write = vk::WriteDescriptorSet::builder()
                        .dst_set(*descriptor_set)
                        .dst_binding(binding_key.dst_binding)
                        //.dst_array_element(element_key.dst_array_element)
                        .dst_array_element(0)
                        .descriptor_type(binding_buffers.buffer_info.descriptor_type.into())
                        .buffer_info(&*write_descriptor_buffer_infos.last().unwrap())
                        .build();

                    descriptor_writes.push(descriptor_set_write);

                    offset += binding_buffers.buffer_info.per_descriptor_stride as u64;
                }
            }
        }

        unsafe {
            device_context
                .device()
                .update_descriptor_sets(&descriptor_writes, &[]);
        }

        Ok(RegisteredDescriptorSetPoolChunk {
            descriptor_set_layout,
            pool,
            descriptor_sets,
            pending_set_writes: Default::default(),
            pending_buffer_writes: Default::default(),
            buffers,
        })
    }

    pub(super) fn destroy(
        &mut self,
        pool_allocator: &mut VkDescriptorPoolAllocator,
        buffer_drop_sink: &mut VkResourceDropSink<ManuallyDrop<VkBuffer>>,
    ) {
        pool_allocator.retire_pool(self.pool);
        for (key, buffer_set) in self.buffers.buffer_sets.drain() {
            for buffer in buffer_set.buffers {
                buffer_drop_sink.retire(buffer);
            }
        }
    }

    pub(super) fn schedule_write_set(
        &mut self,
        slab_key: RawSlabKey<RegisteredDescriptorSet>,
        mut write_set: DescriptorSetWriteSet,
        frame_in_flight_index: FrameInFlightIndex,
    ) -> Vec<vk::DescriptorSet> {
        log::trace!(
            "Schedule a write for descriptor set {:?} on frame in flight index {} layout {:?}",
            slab_key,
            frame_in_flight_index,
            self.descriptor_set_layout
        );
        //log::trace!("{:#?}", write_set);

        // Use frame_in_flight_index for the live_until_frame because every update, we immediately
        // increment the frame and *then* do updates. So by setting it to the pre-next-update
        // frame_in_flight_index, this will make the write stick around for this and the next
        // MAX_FRAMES_IN_FLIGHT frames
        let pending_write = PendingDescriptorSetWriteSet {
            slab_key,
            write_set,
            live_until_frame: super::add_to_frame_in_flight_index(
                frame_in_flight_index,
                MAX_FRAMES_IN_FLIGHT as u32,
            ),
        };

        //TODO: Consider pushing these into a hashmap for the frame and let the pending write array
        // be a list of hashmaps
        self.pending_set_writes.push_back(pending_write);

        let descriptor_index = slab_key.index() % MAX_DESCRIPTORS_PER_POOL;
        self.descriptor_sets
            .iter()
            .map(|x| x[descriptor_index as usize])
            .collect()
    }

    pub(super) fn schedule_write_buffer(
        &mut self,
        slab_key: RawSlabKey<RegisteredDescriptorSet>,
        mut write_buffer: DescriptorSetWriteBuffer,
        frame_in_flight_index: FrameInFlightIndex,
    ) -> Vec<vk::DescriptorSet> {
        log::trace!("Schedule a buffer write for descriptor set {:?} on frame in flight index {} layout {:?}", slab_key, frame_in_flight_index, self.descriptor_set_layout);
        //log::trace!("{:#?}", write_buffer);

        // Use frame_in_flight_index for the live_until_frame because every update, we immediately
        // increment the frame and *then* do updates. So by setting it to the pre-next-update
        // frame_in_flight_index, this will make the write stick around for this and the next
        // MAX_FRAMES_IN_FLIGHT frames
        let pending_write = PendingDescriptorSetWriteBuffer {
            slab_key,
            write_buffer,
            live_until_frame: super::add_to_frame_in_flight_index(
                frame_in_flight_index,
                MAX_FRAMES_IN_FLIGHT as u32,
            ),
        };

        //TODO: Consider pushing these into a hashmap for the frame and let the pending write array
        // be a list of hashmaps
        self.pending_buffer_writes.push_back(pending_write);

        let descriptor_index = slab_key.index() % MAX_DESCRIPTORS_PER_POOL;
        self.descriptor_sets
            .iter()
            .map(|x| x[descriptor_index as usize])
            .collect()
    }

    pub(super) fn update(
        &mut self,
        device_context: &VkDeviceContext,
        frame_in_flight_index: FrameInFlightIndex,
    ) {
        // This function is a bit tricky unfortunately. We need to build a list of vk::WriteDescriptorSet
        // but this struct has a pointer to data in image_infos/buffer_infos. To deal with this, we
        // need to push the temporary lists of these infos into these lists. This way they don't
        // drop out of scope while we are using them. Ash does do some lifetime tracking, but once
        // you call build() it completely trusts that any pointers it holds will stay valid. So
        // while these lists are mutable to allow pushing data in, the Vecs inside must not be modified.
        let mut vk_image_infos = vec![];
        //let mut vk_buffer_infos = vec![];

        #[derive(PartialEq, Eq, Hash, Debug)]
        struct SlabElementKey(RawSlabKey<RegisteredDescriptorSet>, DescriptorSetElementKey);

        // Flatten the vec of hash maps into a single hashmap. This eliminates any duplicate
        // sets with the most recent set taking precedence
        let mut all_set_writes = FnvHashMap::default();
        for pending_write in &self.pending_set_writes {
            for (key, value) in &pending_write.write_set.elements {
                all_set_writes.insert(SlabElementKey(pending_write.slab_key, *key), value);
            }
        }

        let mut write_builders = vec![];
        for (key, element) in all_set_writes {
            let slab_key = key.0;
            let element_key = key.1;

            log::trace!("Process descriptor set pending_write for {:?} {:?}. Frame in flight: {} layout {:?}", slab_key, element_key, frame_in_flight_index, self.descriptor_set_layout);
            //log::trace!("{:#?}", element);

            let descriptor_set_index = slab_key.index() % MAX_DESCRIPTORS_PER_POOL;
            let descriptor_set =
                self.descriptor_sets[frame_in_flight_index as usize][descriptor_set_index as usize];

            let mut builder = vk::WriteDescriptorSet::builder()
                .dst_set(descriptor_set)
                .dst_binding(element_key.dst_binding)
                //.dst_array_element(element_key.dst_array_element)
                .dst_array_element(0)
                .descriptor_type(element.descriptor_type.into());

            //TODO: https://www.khronos.org/registry/vulkan/specs/1.2-extensions/man/html/VkWriteDescriptorSet.html has
            // info on what fields need to be set based on descriptor type
            let mut image_infos = Vec::with_capacity(element.image_info.len());
            if !element.image_info.is_empty() {
                for image_info in &element.image_info {

                    if element.has_immutable_sampler
                        && element.descriptor_type == dsc::DescriptorType::Sampler
                    {
                        // Skip any sampler bindings if the binding is populated with an immutable sampler
                        continue;
                    }

                    if image_info.sampler.is_none() && image_info.image_view.is_none() {
                        // Don't bind anything that has both a null sampler and image_view
                        continue;
                    }

                    let mut image_info_builder = vk::DescriptorImageInfo::builder();
                    image_info_builder =
                        image_info_builder.image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                    if let Some(image_view) = &image_info.image_view {
                        image_info_builder =
                            image_info_builder.image_view(image_view.get_raw().image_view);
                    }

                    // Skip adding samplers if the binding is populated with an immutable sampler
                    // (this case is hit when using CombinedImageSampler)
                    if !element.has_immutable_sampler {
                        if let Some(sampler) = &image_info.sampler {
                            image_info_builder = image_info_builder.sampler(sampler.get_raw());
                        }
                    }

                    image_infos.push(image_info_builder.build());
                }

                builder = builder.image_info(&image_infos);
            }

            //TODO: DIRTY HACK
            if builder.descriptor_count == 0 {
                continue;
            }

            write_builders.push(builder.build());
            vk_image_infos.push(image_infos);
        }

        if !write_builders.is_empty() {
            unsafe {
                device_context
                    .device()
                    .update_descriptor_sets(&write_builders, &[]);
            }
        }

        let mut all_buffer_writes = FnvHashMap::default();
        for pending_buffer_write in &self.pending_buffer_writes {
            for (key, value) in &pending_buffer_write.write_buffer.elements {
                all_buffer_writes
                    .insert(SlabElementKey(pending_buffer_write.slab_key, *key), value);
            }
        }

        for (key, data) in all_buffer_writes {
            let slab_key = key.0;
            let element_key = key.1;

            log::trace!(
                "Process buffer pending_write for {:?} {:?}. Frame in flight: {} layout: {:?}",
                slab_key,
                element_key,
                frame_in_flight_index,
                self.descriptor_set_layout
            );
            log::trace!("{} bytes", data.len());

            let mut buffer = self.buffers.buffer_sets.get_mut(&element_key).unwrap();
            assert!(data.len() as u32 <= buffer.buffer_info.per_descriptor_size);
            if data.len() as u32 != buffer.buffer_info.per_descriptor_size {
                log::warn!(
                    "Wrote {} bytes to a descriptor set buffer that holds {} bytes layout: {:?}",
                    data.len(),
                    buffer.buffer_info.per_descriptor_size,
                    self.descriptor_set_layout
                );
            }

            let descriptor_set_index = slab_key.index() % MAX_DESCRIPTORS_PER_POOL;
            let offset = buffer.buffer_info.per_descriptor_stride * descriptor_set_index;

            let buffer = &mut buffer.buffers[frame_in_flight_index as usize];

            buffer.write_to_host_visible_buffer_with_offset(&data, offset as u64);
        }

        // Determine how many writes we can drain
        let mut pending_set_writes_to_drain = 0;
        for pending_write in &self.pending_set_writes {
            // If frame_in_flight_index matches or exceeds live_until_frame, then the result will be a very
            // high value due to wrapping a negative value to u32::MAX
            if pending_write.live_until_frame == frame_in_flight_index {
                pending_set_writes_to_drain += 1;
            } else {
                break;
            }
        }

        if pending_set_writes_to_drain > 0 {
            log::trace!(
                "Drop {} set writes on frame in flight index {} layout {:?}",
                pending_set_writes_to_drain,
                frame_in_flight_index,
                self.descriptor_set_layout
            );
        }

        // Drop any writes that have lived long enough to apply to the descriptor set for each frame
        self.pending_set_writes
            .drain(0..pending_set_writes_to_drain);

        // Determine how many writes we can drain
        let mut pending_buffer_writes_to_drain = 0;
        for pending_write in &self.pending_buffer_writes {
            // If frame_in_flight_index matches or exceeds live_until_frame, then the result will be a very
            // high value due to wrapping a negative value to u32::MAX
            if pending_write.live_until_frame == frame_in_flight_index {
                pending_buffer_writes_to_drain += 1;
            } else {
                break;
            }
        }

        if pending_buffer_writes_to_drain > 0 {
            log::trace!(
                "Drop {} buffer writes on frame in flight index {} layout {:?}",
                pending_buffer_writes_to_drain,
                frame_in_flight_index,
                self.descriptor_set_layout
            );
        }

        // Drop any writes that have lived long enough to apply to the descriptor set for each frame
        self.pending_buffer_writes
            .drain(0..pending_buffer_writes_to_drain);
    }
}