use renderer_shell_vulkan::{
    VkTransferUploadState, VkDeviceContext, VkTransferUpload, VkImage, VkBuffer,
};
use crossbeam_channel::{Sender, Receiver};
use ash::prelude::VkResult;
use crate::image_utils::{enqueue_load_images, DecodedTexture, enqueue_load_buffers};
use std::mem::ManuallyDrop;
use atelier_assets::loader::{LoadHandle, AssetLoadOp};
use ash::vk;
use crate::resources::load_queue::LoadRequest;
use crate::assets::ImageAssetData;
use crate::assets::BufferAssetData;
use crate::assets::{ImageAsset, BufferAsset};

//
// Ghetto futures - UploadOp is used to signal completion and UploadOpAwaiter is used to check the result
//
pub enum UploadOpResult<ResourceT, AssetT> {
    UploadError(LoadHandle),
    UploadComplete(AssetLoadOp, Sender<AssetT>, ResourceT),
    UploadDrop(LoadHandle),
}

pub struct UploadOp<ResourceT, AssetT> {
    load_handle: LoadHandle,
    asset_sender: Option<Sender<AssetT>>, // This sends back to the asset storage, we just pass it along
    sender: Option<Sender<UploadOpResult<ResourceT, AssetT>>>, // This sends back to the resource manager to finalize the load
}

impl<ResourceT, AssetT> UploadOp<ResourceT, AssetT> {
    pub fn new(
        load_handle: LoadHandle,
        asset_sender: Sender<AssetT>,
        sender: Sender<UploadOpResult<ResourceT, AssetT>>,
    ) -> Self {
        Self {
            load_handle,
            asset_sender: Some(asset_sender),
            sender: Some(sender),
        }
    }

    pub fn complete(
        mut self,
        image: ResourceT,
        load_op: AssetLoadOp,
    ) {
        let _ = self
            .sender
            .as_ref()
            .unwrap()
            .send(UploadOpResult::UploadComplete(
                load_op,
                self.asset_sender.take().unwrap(),
                image,
            ));
        self.sender = None;
    }

    pub fn error(mut self) {
        let _ = self
            .sender
            .as_ref()
            .unwrap()
            .send(UploadOpResult::UploadError(self.load_handle));
        self.sender = None;
    }
}

impl<ResourceT, AssetT> Drop for UploadOp<ResourceT, AssetT> {
    fn drop(&mut self) {
        if let Some(ref sender) = self.sender {
            let _ = sender.send(UploadOpResult::UploadDrop(self.load_handle));
        }
    }
}

pub type ImageUploadOpResult = UploadOpResult<VkImage, ImageAsset>;
pub type ImageUploadOp = UploadOp<VkImage, ImageAsset>;

pub type BufferUploadOpResult = UploadOpResult<VkBuffer, BufferAsset>;
pub type BufferUploadOp = UploadOp<VkBuffer, BufferAsset>;

//
// Represents a single request inserted into the upload queue that hasn't started yet
//
//TODO: Make a helper object that carries an Arc<Receiver> that can be called
pub struct PendingImageUpload {
    pub load_op: AssetLoadOp,
    pub upload_op: ImageUploadOp,
    pub texture: DecodedTexture,
}

pub struct PendingBufferUpload {
    pub load_op: AssetLoadOp,
    pub upload_op: BufferUploadOp,
    pub data: Vec<u8>,
}

//
// Represents a single request that the upload queue has started
//
struct InFlightImageUpload {
    load_op: AssetLoadOp,
    upload_op: ImageUploadOp,
    image: ManuallyDrop<VkImage>,
}

pub struct InFlightBufferUpload {
    load_op: AssetLoadOp,
    upload_op: BufferUploadOp,
    buffer: ManuallyDrop<VkBuffer>,
}

//
// Represents a batch of requests that has been started, contains multiple InFlightImageUpload and
// InFlightBufferUploads
//

// The result from polling a single upload (which may contain multiple images in it)
pub enum InProgressUploadPollResult {
    Pending,
    Complete,
    Error,
    Destroyed,
}

// This is an inner of InProgressImageUpload - it is wrapped in a Option to avoid borrowing issues
// when polling by allowing us to temporarily take ownership of it and then put it back
struct InProgressUploadInner {
    image_uploads: Vec<InFlightImageUpload>,
    buffer_uploads: Vec<InFlightBufferUpload>,
    upload: VkTransferUpload,
}

// A single upload which may contain multiple images
struct InProgressUpload {
    inner: Option<InProgressUploadInner>,
}

impl InProgressUpload {
    pub fn new(
        image_uploads: Vec<InFlightImageUpload>,
        buffer_uploads: Vec<InFlightBufferUpload>,
        upload: VkTransferUpload,
    ) -> Self {
        let inner = InProgressUploadInner {
            image_uploads,
            buffer_uploads,
            upload,
        };

        InProgressUpload { inner: Some(inner) }
    }

    // The main state machine for an upload:
    // - Submits on the transfer queue and waits
    // - Submits on the graphics queue and waits
    //
    // Calls load_op.complete() or load_op.error() as appropriate
    pub fn poll_load(
        &mut self,
        device_context: &VkDeviceContext,
    ) -> InProgressUploadPollResult {
        loop {
            if let Some(mut inner) = self.take_inner() {
                match inner.upload.state() {
                    Ok(state) => match state {
                        VkTransferUploadState::Writable => {
                            //log::trace!("VkTransferUploadState::Writable");
                            inner
                                .upload
                                .submit_transfer(&device_context.queues().transfer_queue)
                                .unwrap();
                            self.inner = Some(inner);
                        }
                        VkTransferUploadState::SentToTransferQueue => {
                            //log::trace!("VkTransferUploadState::SentToTransferQueue");
                            self.inner = Some(inner);
                            break InProgressUploadPollResult::Pending;
                        }
                        VkTransferUploadState::PendingSubmitDstQueue => {
                            //log::trace!("VkTransferUploadState::PendingSubmitDstQueue");
                            inner
                                .upload
                                .submit_dst(&device_context.queues().graphics_queue)
                                .unwrap();
                            self.inner = Some(inner);
                        }
                        VkTransferUploadState::SentToDstQueue => {
                            //log::trace!("VkTransferUploadState::SentToDstQueue");
                            self.inner = Some(inner);
                            break InProgressUploadPollResult::Pending;
                        }
                        VkTransferUploadState::Complete => {
                            //log::trace!("VkTransferUploadState::Complete");
                            for mut upload in inner.image_uploads {
                                let image = unsafe { ManuallyDrop::take(&mut upload.image) };
                                upload.upload_op.complete(image, upload.load_op);
                            }

                            for mut upload in inner.buffer_uploads {
                                let buffer = unsafe { ManuallyDrop::take(&mut upload.buffer) };
                                upload.upload_op.complete(buffer, upload.load_op);
                            }

                            break InProgressUploadPollResult::Complete;
                        }
                    },
                    Err(err) => {
                        for mut upload in inner.image_uploads {
                            upload.load_op.error(err);
                            upload.upload_op.error();
                            unsafe {
                                ManuallyDrop::drop(&mut upload.image);
                            }
                        }

                        for mut upload in inner.buffer_uploads {
                            upload.load_op.error(err);
                            upload.upload_op.error();
                            unsafe {
                                ManuallyDrop::drop(&mut upload.buffer);
                            }
                        }

                        break InProgressUploadPollResult::Error;
                    }
                }
            } else {
                break InProgressUploadPollResult::Destroyed;
            }
        }
    }

    // Allows taking ownership of the inner object
    fn take_inner(&mut self) -> Option<InProgressUploadInner> {
        let mut inner = None;
        std::mem::swap(&mut self.inner, &mut inner);
        inner
    }
}

impl Drop for InProgressUpload {
    fn drop(&mut self) {
        if let Some(mut inner) = self.take_inner() {
            for image in &mut inner.image_uploads {
                unsafe {
                    ManuallyDrop::drop(&mut image.image);
                }
            }

            for buffer in &mut inner.buffer_uploads {
                unsafe {
                    ManuallyDrop::drop(&mut buffer.buffer);
                }
            }
        }
    }
}

//
// Receives sets of images that need to be uploaded and kicks off the upload. Responsible for
// batching image updates together into uploads
//
pub struct UploadQueue {
    device_context: VkDeviceContext,

    // For enqueueing images to upload
    pending_image_tx: Sender<PendingImageUpload>,
    pending_image_rx: Receiver<PendingImageUpload>,

    // For enqueueing buffers to upload
    pending_buffer_tx: Sender<PendingBufferUpload>,
    pending_buffer_rx: Receiver<PendingBufferUpload>,

    // These are uploads that are currently in progress
    uploads_in_progress: Vec<InProgressUpload>,
}

impl UploadQueue {
    pub fn new(device_context: &VkDeviceContext) -> Self {
        let (pending_image_tx, pending_image_rx) = crossbeam_channel::unbounded();
        let (pending_buffer_tx, pending_buffer_rx) = crossbeam_channel::unbounded();

        UploadQueue {
            device_context: device_context.clone(),
            pending_image_tx,
            pending_image_rx,
            pending_buffer_tx,
            pending_buffer_rx,
            uploads_in_progress: Default::default(),
        }
    }

    pub fn pending_image_tx(&self) -> &Sender<PendingImageUpload> {
        &self.pending_image_tx
    }

    pub fn pending_buffer_tx(&self) -> &Sender<PendingBufferUpload> {
        &self.pending_buffer_tx
    }

    fn start_new_image_uploads(
        &mut self,
        upload: &mut VkTransferUpload,
    ) -> VkResult<Vec<InFlightImageUpload>> {
        let mut ops = vec![];
        let mut decoded_textures = vec![];

        for pending_upload in self.pending_image_rx.try_iter() {
            log::trace!(
                "start image upload size: {}",
                pending_upload.texture.data.len()
            );
            ops.push((pending_upload.load_op, pending_upload.upload_op));
            decoded_textures.push(pending_upload.texture);

            //TODO: Handle budgeting how much we can upload at once
        }

        if decoded_textures.is_empty() {
            return Ok(vec![]);
        }

        let images = enqueue_load_images(
            &self.device_context,
            upload,
            self.device_context
                .queue_family_indices()
                .transfer_queue_family_index,
            self.device_context
                .queue_family_indices()
                .graphics_queue_family_index,
            &decoded_textures,
        )?;

        let mut in_flight_uploads = Vec::with_capacity(ops.len());
        for (op, image) in ops.into_iter().zip(images) {
            in_flight_uploads.push(InFlightImageUpload {
                load_op: op.0,
                upload_op: op.1,
                image,
            });
        }

        Ok(in_flight_uploads)
    }

    fn start_new_buffer_uploads(
        &mut self,
        upload: &mut VkTransferUpload,
    ) -> VkResult<Vec<InFlightBufferUpload>> {
        let mut ops = vec![];
        let mut buffer_data = vec![];

        for pending_upload in self.pending_buffer_rx.try_iter() {
            log::trace!("start buffer upload size: {}", pending_upload.data.len());
            ops.push((pending_upload.load_op, pending_upload.upload_op));
            buffer_data.push(pending_upload.data);

            //TODO: Handle budgeting how much we can upload at once
        }

        if buffer_data.is_empty() {
            return Ok(vec![]);
        }

        let buffers = enqueue_load_buffers(
            &self.device_context,
            upload,
            self.device_context
                .queue_family_indices()
                .transfer_queue_family_index,
            self.device_context
                .queue_family_indices()
                .graphics_queue_family_index,
            &buffer_data,
        )?;

        let mut in_flight_uploads = Vec::with_capacity(ops.len());
        for (op, buffer) in ops.into_iter().zip(buffers) {
            in_flight_uploads.push(InFlightBufferUpload {
                load_op: op.0,
                upload_op: op.1,
                buffer,
            });
        }

        Ok(in_flight_uploads)
    }

    fn start_new_uploads(&mut self) -> VkResult<()> {
        if self.pending_image_rx.is_empty() && self.pending_buffer_rx.is_empty() {
            return Ok(());
        }

        let mut upload = VkTransferUpload::new(
            &self.device_context,
            self.device_context
                .queue_family_indices()
                .transfer_queue_family_index,
            self.device_context
                .queue_family_indices()
                .graphics_queue_family_index,
            1024 * 1024 * 256,
        )?;

        let in_flight_image_uploads = self.start_new_image_uploads(&mut upload)?;
        let in_flight_buffer_uploads = self.start_new_buffer_uploads(&mut upload)?;

        if !in_flight_image_uploads.is_empty() || !in_flight_buffer_uploads.is_empty() {
            upload.submit_transfer(&self.device_context.queues().transfer_queue)?;
            self.uploads_in_progress.push(InProgressUpload::new(
                in_flight_image_uploads,
                in_flight_buffer_uploads,
                upload,
            ));
        }

        Ok(())
    }

    fn update_existing_uploads(&mut self) {
        // iterate backwards so we can use swap_remove
        for i in (0..self.uploads_in_progress.len()).rev() {
            let result = self.uploads_in_progress[i].poll_load(&self.device_context);
            match result {
                InProgressUploadPollResult::Pending => {
                    // do nothing
                }
                InProgressUploadPollResult::Complete => {
                    //load_op.complete() is called by poll_load
                    self.uploads_in_progress.swap_remove(i);
                }
                InProgressUploadPollResult::Error => {
                    //load_op.error() is called by poll_load
                    self.uploads_in_progress.swap_remove(i);
                }
                InProgressUploadPollResult::Destroyed => {
                    // not expected - this only occurs if polling the upload when it is already in a complete or error state
                    unreachable!();
                }
            }
        }
    }

    pub fn update(&mut self) -> VkResult<()> {
        self.start_new_uploads()?;
        self.update_existing_uploads();
        Ok(())
    }
}

pub struct UploadManager {
    upload_queue: UploadQueue,

    pub image_upload_result_tx: Sender<ImageUploadOpResult>,
    pub image_upload_result_rx: Receiver<ImageUploadOpResult>,

    pub buffer_upload_result_tx: Sender<BufferUploadOpResult>,
    pub buffer_upload_result_rx: Receiver<BufferUploadOpResult>,
}

impl UploadManager {
    pub fn new(device_context: &VkDeviceContext) -> Self {
        let (image_upload_result_tx, image_upload_result_rx) = crossbeam_channel::unbounded();
        let (buffer_upload_result_tx, buffer_upload_result_rx) = crossbeam_channel::unbounded();

        UploadManager {
            upload_queue: UploadQueue::new(device_context),
            image_upload_result_rx,
            image_upload_result_tx,
            buffer_upload_result_rx,
            buffer_upload_result_tx,
        }
    }

    pub fn update(&mut self) -> VkResult<()> {
        self.upload_queue.update()
    }

    pub fn upload_image(
        &self,
        request: LoadRequest<ImageAssetData, ImageAsset>,
    ) -> VkResult<()> {
        let mips = crate::image_utils::default_mip_settings_for_image(
            request.asset.width,
            request.asset.height,
        );

        let color_space = request.asset.color_space.into();

        let decoded_texture = DecodedTexture {
            width: request.asset.width,
            height: request.asset.height,
            mips,
            color_space,
            data: request.asset.data,
        };

        self.upload_queue
            .pending_image_tx()
            .send(PendingImageUpload {
                load_op: request.load_op,
                upload_op: UploadOp::new(
                    request.load_handle,
                    request.result_tx,
                    self.image_upload_result_tx.clone(),
                ),
                texture: decoded_texture,
            })
            .map_err(|_err| {
                log::error!("Could not enqueue image upload");
                vk::Result::ERROR_UNKNOWN
            })
    }

    pub fn upload_buffer(
        &self,
        request: LoadRequest<BufferAssetData, BufferAsset>,
    ) -> VkResult<()> {
        self.upload_queue
            .pending_buffer_tx()
            .send(PendingBufferUpload {
                load_op: request.load_op,
                upload_op: UploadOp::new(
                    request.load_handle,
                    request.result_tx,
                    self.buffer_upload_result_tx.clone(),
                ),
                data: request.asset.data,
            })
            .map_err(|_err| {
                log::error!("Could not enqueue buffer upload");
                vk::Result::ERROR_UNKNOWN
            })
    }
}
