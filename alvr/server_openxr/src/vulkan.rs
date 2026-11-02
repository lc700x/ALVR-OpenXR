//! Vulkan swapchain interop for the Windows OpenXR runtime.
//!
//! Vulkan entry points are loaded dynamically through ash. The application
//! creates its own instance, device and queue; runtime submissions use only
//! that bound queue and are made from OpenXR calls that permit queue access.

use ash::vk::Handle;
use ash::{vk, Entry};
use std::{
    ffi::CStr,
    sync::atomic::{AtomicUsize, Ordering},
};

pub const VK_FORMAT_R8G8B8A8_UNORM: i64 = vk::Format::R8G8B8A8_UNORM.as_raw() as i64;
pub const VK_FORMAT_R8G8B8A8_SRGB: i64 = vk::Format::R8G8B8A8_SRGB.as_raw() as i64;
pub const VK_FORMAT_B8G8R8A8_UNORM: i64 = vk::Format::B8G8R8A8_UNORM.as_raw() as i64;
pub const VK_FORMAT_B8G8R8A8_SRGB: i64 = vk::Format::B8G8R8A8_SRGB.as_raw() as i64;

pub fn vk_format_to_dxgi(vk_format: i64) -> i32 {
    match vk_format {
        VK_FORMAT_R8G8B8A8_UNORM | VK_FORMAT_R8G8B8A8_SRGB => 27,
        VK_FORMAT_B8G8R8A8_UNORM | VK_FORMAT_B8G8R8A8_SRGB => 90,
        _ => 0,
    }
}

pub fn vk_format_to_dxgi_typed(vk_format: i64) -> i32 {
    match vk_format {
        VK_FORMAT_R8G8B8A8_UNORM => 28,
        VK_FORMAT_R8G8B8A8_SRGB => 29,
        VK_FORMAT_B8G8R8A8_UNORM => 87,
        VK_FORMAT_B8G8R8A8_SRGB => 91,
        _ => 0,
    }
}

fn unorm_sibling(format: vk::Format) -> vk::Format {
    match format {
        vk::Format::R8G8B8A8_SRGB => vk::Format::R8G8B8A8_UNORM,
        vk::Format::B8G8R8A8_SRGB => vk::Format::B8G8R8A8_UNORM,
        other => other,
    }
}

pub fn is_supported_format(format: i64) -> bool {
    matches!(
        format,
        VK_FORMAT_R8G8B8A8_UNORM
            | VK_FORMAT_R8G8B8A8_SRGB
            | VK_FORMAT_B8G8R8A8_UNORM
            | VK_FORMAT_B8G8R8A8_SRGB
    )
}

/// Runtime resources used to submit image ownership/layout barriers.
#[derive(Clone, Copy, Debug, Default)]
pub struct CommandResources {
    pub pool: u64,
    pub command_buffer: usize,
    pub fence: u64,
}

/// Ash dispatch tables are cached for the lifetime of the OpenXR session. Vulkan handles are
/// borrowed from the application and are never destroyed by this wrapper.
pub struct VulkanFns {
    _entry: Entry,
    instance: ash::Instance,
    device: ash::Device,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    queue_family_index: u32,
}

pub struct PerfScope {
    name: &'static str,
    start: Option<std::time::Instant>,
}

impl PerfScope {
    pub fn new(name: &'static str) -> Self {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let enabled =
            *ENABLED.get_or_init(|| std::env::var("ALVR_OPENXR_PERF").as_deref() == Ok("1"));
        Self {
            name,
            start: enabled.then(std::time::Instant::now),
        }
    }
}

impl Drop for PerfScope {
    fn drop(&mut self) {
        use std::sync::{Mutex, OnceLock};
        static SAMPLES: OnceLock<Mutex<std::collections::HashMap<&'static str, Vec<f64>>>> =
            OnceLock::new();
        let Some(start) = self.start else { return };
        let mut all = SAMPLES
            .get_or_init(|| Mutex::new(Default::default()))
            .lock()
            .unwrap();
        let samples = all.entry(self.name).or_default();
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        if samples.len() == 300 {
            samples.sort_by(f64::total_cmp);
            eprintln!(
                "[alvr] perf {} ms p50={:.3} p95={:.3} max={:.3}",
                self.name, samples[150], samples[285], samples[299]
            );
            samples.clear();
        }
    }
}

impl VulkanFns {
    /// # Safety
    /// All handles must belong to one live Vulkan instance/device and the queue
    /// family/index must be the OpenXR graphics binding queue.
    pub unsafe fn load(
        instance: usize,
        physical_device: usize,
        device: usize,
        queue_family_index: u32,
        queue_index: u32,
    ) -> Option<Self> {
        if instance == 0 || physical_device == 0 || device == 0 {
            return None;
        }
        let entry = unsafe { Entry::load().ok()? };
        let instance_handle = vk::Instance::from_raw(instance as u64);
        let instance_dispatch = unsafe { ash::Instance::load(entry.static_fn(), instance_handle) };
        let device_handle = vk::Device::from_raw(device as u64);
        let device_dispatch =
            unsafe { ash::Device::load(instance_dispatch.fp_v1_0(), device_handle) };
        let queue = unsafe { device_dispatch.get_device_queue(queue_family_index, queue_index) };
        if queue == vk::Queue::null() {
            return None;
        }
        Some(Self {
            _entry: entry,
            instance: instance_dispatch,
            device: device_dispatch,
            physical_device: vk::PhysicalDevice::from_raw(physical_device as u64),
            queue,
            queue_family_index,
        })
    }

    pub fn supports_external_image(
        &self,
        format: i64,
        width: u32,
        height: u32,
        array_layers: u32,
        usage: vk::ImageUsageFlags,
    ) -> bool {
        let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE);
        let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(vk::Format::from_raw(format as i32))
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
            .push_next(&mut external_info);
        let mut external_properties = vk::ExternalImageFormatProperties::default();
        let mut properties =
            vk::ImageFormatProperties2::default().push_next(&mut external_properties);
        let properties2 =
            ash::khr::get_physical_device_properties2::Instance::new(&self._entry, &self.instance);
        let result = unsafe {
            (properties2
                .fp()
                .get_physical_device_image_format_properties2_khr)(
                self.physical_device,
                &format_info,
                &mut properties,
            )
        };
        let image_properties = properties.image_format_properties;
        // The pNext builders retain mutable borrows of the chained structures.
        // End those borrows before reading the returned external-memory fields.
        result == vk::Result::SUCCESS
            && external_properties
                .external_memory_properties
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
            && external_properties
                .external_memory_properties
                .compatible_handle_types
                .contains(vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE)
            && image_properties.max_extent.width >= width
            && image_properties.max_extent.height >= height
            && image_properties.max_array_layers >= array_layers
            && image_properties
                .sample_counts
                .contains(vk::SampleCountFlags::TYPE_1)
    }

    pub fn create_command_resources(
        &self,
        count: usize,
    ) -> Result<Vec<CommandResources>, vk::Result> {
        let mut resources = Vec::with_capacity(count);
        for _ in 0..count {
            let pool_info = vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                .queue_family_index(self.queue_family_index);
            let pool = match unsafe { self.device.create_command_pool(&pool_info, None) } {
                Ok(pool) => pool,
                Err(error) => {
                    unsafe { self.destroy_command_resources(&resources) };
                    return Err(error);
                }
            };
            let allocation = vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let command_buffer = match unsafe { self.device.allocate_command_buffers(&allocation) }
            {
                Ok(buffers) => buffers[0],
                Err(error) => {
                    unsafe { self.device.destroy_command_pool(pool, None) };
                    unsafe { self.destroy_command_resources(&resources) };
                    return Err(error);
                }
            };
            let fence = match unsafe {
                self.device
                    .create_fence(&vk::FenceCreateInfo::default(), None)
            } {
                Ok(fence) => fence,
                Err(error) => {
                    unsafe { self.device.destroy_command_pool(pool, None) };
                    unsafe { self.destroy_command_resources(&resources) };
                    return Err(error);
                }
            };
            resources.push(CommandResources {
                pool: pool.as_raw(),
                command_buffer: command_buffer.as_raw() as usize,
                fence: fence.as_raw(),
            });
        }
        Ok(resources)
    }

    /// # Safety
    /// Resources must have been created by this device and no longer be in use.
    pub unsafe fn destroy_command_resources(&self, resources: &[CommandResources]) {
        for item in resources {
            unsafe {
                self.device
                    .destroy_fence(vk::Fence::from_raw(item.fence), None);
                self.device
                    .destroy_command_pool(vk::CommandPool::from_raw(item.pool), None);
            }
        }
    }

    /// Submit external-to-application ownership transfer on the bound queue.
    /// xrWaitSwapchainImage waits this submission's fence.
    ///
    /// # Safety
    /// Image, memory and command resources must be live and owned by this device.
    pub unsafe fn acquire_image(
        &self,
        image: u64,
        memory: u64,
        array_layers: u32,
        command: CommandResources,
        external: bool,
        first_use: bool,
        fence_pending: bool,
    ) -> Result<(), vk::Result> {
        let fence = vk::Fence::from_raw(command.fence);
        if fence_pending {
            unsafe { self.device.wait_for_fences(&[fence], true, u64::MAX)? };
            unsafe { self.device.reset_fences(&[fence])? };
        }
        let pool = vk::CommandPool::from_raw(command.pool);
        unsafe {
            self.device
                .reset_command_pool(pool, vk::CommandPoolResetFlags::empty())?
        };
        let cmd = vk::CommandBuffer::from_raw(command.command_buffer as u64);
        unsafe {
            self.device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?
        };
        let barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .old_layout(if first_use {
                vk::ImageLayout::UNDEFINED
            } else if external {
                vk::ImageLayout::GENERAL
            } else {
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
            })
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .src_queue_family_index(if external {
                vk::QUEUE_FAMILY_EXTERNAL
            } else {
                vk::QUEUE_FAMILY_IGNORED
            })
            .dst_queue_family_index(if external {
                self.queue_family_index
            } else {
                vk::QUEUE_FAMILY_IGNORED
            })
            .image(vk::Image::from_raw(image))
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .base_mip_level(0)
                    .level_count(1)
                    .base_array_layer(0)
                    .layer_count(array_layers.max(1)),
            );
        unsafe {
            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
            self.device.end_command_buffer(cmd)?;
        }

        if external {
            let acquire_syncs = [vk::DeviceMemory::from_raw(memory)];
            let acquire_keys = [0];
            let acquire_timeouts = [u32::MAX];
            let mut keyed = vk::Win32KeyedMutexAcquireReleaseInfoKHR::default()
                .acquire_syncs(&acquire_syncs)
                .acquire_keys(&acquire_keys)
                .acquire_timeouts(&acquire_timeouts);
            let submit = vk::SubmitInfo::default()
                .push_next(&mut keyed)
                .command_buffers(std::slice::from_ref(&cmd));
            unsafe { self.device.queue_submit(self.queue, &[submit], fence) }
        } else {
            let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
            unsafe { self.device.queue_submit(self.queue, &[submit], fence) }
        }
    }

    /// Submit application-to-external ownership transfer and keyed-mutex
    /// release on the bound application queue.
    ///
    /// # Safety
    /// Image, memory and command resources must be live and owned by this device.
    pub unsafe fn release_image(
        &self,
        image: u64,
        memory: u64,
        array_layers: u32,
        command: CommandResources,
        external: bool,
        fence_pending: bool,
    ) -> Result<(), vk::Result> {
        let fence = vk::Fence::from_raw(command.fence);
        if fence_pending {
            unsafe { self.device.wait_for_fences(&[fence], true, u64::MAX)? };
            unsafe { self.device.reset_fences(&[fence])? };
        }
        let pool = vk::CommandPool::from_raw(command.pool);
        unsafe {
            self.device
                .reset_command_pool(pool, vk::CommandPoolResetFlags::empty())?
        };
        let cmd = vk::CommandBuffer::from_raw(command.command_buffer as u64);
        unsafe {
            self.device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?
        };
        if external {
            // Swapchain contents may be written by copies/blits as well as rendering.
            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
                .dst_access_mask(vk::AccessFlags::empty())
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(self.queue_family_index)
                .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                .image(vk::Image::from_raw(image))
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .base_mip_level(0)
                        .level_count(1)
                        .base_array_layer(0)
                        .layer_count(array_layers.max(1)),
                );
            unsafe {
                self.device.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier],
                );
            }
        }
        unsafe { self.device.end_command_buffer(cmd)? };

        let release_syncs = [vk::DeviceMemory::from_raw(memory)];
        let release_keys = [0];
        let mut keyed = vk::Win32KeyedMutexAcquireReleaseInfoKHR::default()
            .release_syncs(&release_syncs)
            .release_keys(&release_keys);
        let submit = if external {
            vk::SubmitInfo::default()
                .push_next(&mut keyed)
                .command_buffers(std::slice::from_ref(&cmd))
        } else {
            vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd))
        };
        unsafe { self.device.queue_submit(self.queue, &[submit], fence) }
    }

    /// Wait on an existing fence without accessing the queue.
    pub unsafe fn wait_fence(&self, fence: u64, timeout_ns: u64) -> Result<(), vk::Result> {
        let _perf = PerfScope::new("fence_wait");
        unsafe {
            self.device
                .wait_for_fences(&[vk::Fence::from_raw(fence)], true, timeout_ns)
        }
    }

    /// Read one array slice from a CPU-fallback image and restore its attachment
    /// layout before returning it to the application.
    ///
    /// # Safety
    /// Image is live, released, and belongs to this device.
    pub unsafe fn readback_image(
        &self,
        image: u64,
        width: u32,
        height: u32,
        array_layer: u32,
        command: CommandResources,
        fence_pending: bool,
    ) -> Option<Vec<u8>> {
        if fence_pending {
            unsafe { self.wait_fence(command.fence, u64::MAX).ok()? };
        }
        let byte_size = u64::from(width)
            .checked_mul(u64::from(height))?
            .checked_mul(4)?;
        let buffer_info = vk::BufferCreateInfo::default()
            .size(byte_size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { self.device.create_buffer(&buffer_info, None).ok()? };
        let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let memory_properties = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical_device)
        };
        let Some(memory_type_index) = (0..memory_properties.memory_type_count).find(|&index| {
            requirements.memory_type_bits & (1 << index) != 0
                && memory_properties.memory_types[index as usize]
                    .property_flags
                    .contains(
                        vk::MemoryPropertyFlags::HOST_VISIBLE
                            | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )
        }) else {
            unsafe { self.device.destroy_buffer(buffer, None) };
            return None;
        };
        let allocation = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        let memory = match unsafe { self.device.allocate_memory(&allocation, None) } {
            Ok(memory) => memory,
            Err(_) => {
                unsafe { self.device.destroy_buffer(buffer, None) };
                return None;
            }
        };
        if unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }.is_err() {
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_buffer(buffer, None);
            }
            return None;
        }

        let pool = vk::CommandPool::from_raw(command.pool);
        let cmd = vk::CommandBuffer::from_raw(command.command_buffer as u64);
        if unsafe {
            self.device
                .reset_command_pool(pool, vk::CommandPoolResetFlags::empty())
        }
        .is_err()
        {
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_buffer(buffer, None);
            }
            return None;
        }
        if unsafe {
            self.device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
        }
        .is_err()
        {
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_buffer(buffer, None);
            }
            return None;
        }
        let color_to_transfer = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(vk::Image::from_raw(image))
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .base_mip_level(0)
                    .level_count(1)
                    .base_array_layer(array_layer)
                    .layer_count(1),
            );
        unsafe {
            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[color_to_transfer],
            );
        }
        let region = vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(0)
                    .base_array_layer(array_layer)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            });
        unsafe {
            self.device.cmd_copy_image_to_buffer(
                cmd,
                vk::Image::from_raw(image),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[region],
            );
        }
        let transfer_to_color = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(vk::Image::from_raw(image))
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .base_mip_level(0)
                    .level_count(1)
                    .base_array_layer(array_layer)
                    .layer_count(1),
            );
        unsafe {
            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[transfer_to_color],
            );
        }
        if unsafe { self.device.end_command_buffer(cmd) }.is_err() {
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_buffer(buffer, None);
            }
            return None;
        }
        let fence = vk::Fence::from_raw(command.fence);
        if unsafe { self.device.reset_fences(&[fence]) }.is_err() {
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_buffer(buffer, None);
            }
            return None;
        }
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
        if unsafe { self.device.queue_submit(self.queue, &[submit], fence) }.is_err()
            || unsafe { self.device.wait_for_fences(&[fence], true, u64::MAX) }.is_err()
        {
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_buffer(buffer, None);
            }
            return None;
        }
        let mapped = match unsafe {
            self.device
                .map_memory(memory, 0, byte_size, vk::MemoryMapFlags::empty())
        } {
            Ok(mapped) => mapped,
            Err(_) => {
                unsafe {
                    self.device.destroy_buffer(buffer, None);
                    self.device.free_memory(memory, None);
                }
                return None;
            }
        };
        let mut pixels = vec![0; byte_size as usize];
        unsafe {
            std::ptr::copy_nonoverlapping(mapped.cast::<u8>(), pixels.as_mut_ptr(), pixels.len());
            self.device.unmap_memory(memory);
            self.device.free_memory(memory, None);
            self.device.destroy_buffer(buffer, None);
        }
        Some(pixels)
    }

    /// Create an image for the universal CPU-copy fallback.
    ///
    /// # Safety
    /// The Vulkan handles belong to this device and the format is supported.
    pub unsafe fn create_owned_image(
        &self,
        format: i64,
        width: u32,
        height: u32,
        array_layers: u32,
        usage: vk::ImageUsageFlags,
    ) -> Option<(u64, u64)> {
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::from_raw(format as i32))
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(array_layers.max(1))
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { self.device.create_image(&image_info, None).ok()? };
        let requirements = unsafe { self.device.get_image_memory_requirements(image) };
        let properties = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical_device)
        };
        let Some(memory_type_index) = (0..properties.memory_type_count)
            .find(|&index| requirements.memory_type_bits & (1 << index) != 0)
        else {
            unsafe { self.device.destroy_image(image, None) };
            return None;
        };
        let allocation = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        let memory = match unsafe { self.device.allocate_memory(&allocation, None) } {
            Ok(memory) => memory,
            Err(_) => {
                unsafe { self.device.destroy_image(image, None) };
                return None;
            }
        };
        if unsafe { self.device.bind_image_memory(image, memory, 0) }.is_err() {
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_image(image, None);
            }
            return None;
        }
        Some((image.as_raw(), memory.as_raw()))
    }

    /// # Safety
    /// The handles must be live allocations created or imported on this device.
    pub unsafe fn import_shared_texture(
        &self,
        shared_handle: usize,
        format: i64,
        width: u32,
        height: u32,
        array_layers: u32,
        usage: vk::ImageUsageFlags,
    ) -> Option<(u64, u64)> {
        if !self.supports_external_image(format, width, height, array_layers, usage) {
            eprintln!("[alvr] Vulkan interop: external image format/usage is incompatible");
            return None;
        }
        let format = vk::Format::from_raw(format as i32);
        let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE);
        let views = [format, unorm_sibling(format)];
        let mut format_list = vk::ImageFormatListCreateInfo::default().view_formats(&views);
        let image_info = vk::ImageCreateInfo::default()
            .push_next(&mut external_info)
            .push_next(&mut format_list)
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(array_layers.max(1))
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { self.device.create_image(&image_info, None).ok()? };
        let requirements = unsafe { self.device.get_image_memory_requirements(image) };
        let external_memory =
            ash::khr::external_memory_win32::Device::new(&self.instance, &self.device);
        let mut handle_properties = vk::MemoryWin32HandlePropertiesKHR::default();
        if unsafe {
            external_memory.get_memory_win32_handle_properties(
                vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE,
                shared_handle as vk::HANDLE,
                &mut handle_properties,
            )
        }
        .is_err()
        {
            unsafe { self.device.destroy_image(image, None) };
            return None;
        }
        let memory_type_bits = requirements.memory_type_bits & handle_properties.memory_type_bits;
        let properties = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical_device)
        };
        let Some(memory_type_index) =
            (0..properties.memory_type_count).find(|&index| memory_type_bits & (1 << index) != 0)
        else {
            unsafe { self.device.destroy_image(image, None) };
            return None;
        };

        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import = vk::ImportMemoryWin32HandleInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE)
            .handle(shared_handle as vk::HANDLE);
        let allocation = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut dedicated)
            .push_next(&mut import);
        let memory = match unsafe { self.device.allocate_memory(&allocation, None) } {
            Ok(memory) => memory,
            Err(error) => {
                eprintln!("[alvr] Vulkan import: vkAllocateMemory failed: {error:?}");
                unsafe { self.device.destroy_image(image, None) };
                return None;
            }
        };
        if let Err(error) = unsafe { self.device.bind_image_memory(image, memory, 0) } {
            eprintln!("[alvr] Vulkan import: vkBindImageMemory failed: {error:?}");
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_image(image, None);
            }
            return None;
        }
        Some((image.as_raw(), memory.as_raw()))
    }

    /// # Safety
    /// Handles must be an image and allocation owned by this device and unused.
    pub unsafe fn destroy_image(&self, image: u64, memory: u64) {
        unsafe {
            if image != 0 {
                self.device.destroy_image(vk::Image::from_raw(image), None);
            }
            if memory != 0 {
                self.device
                    .free_memory(vk::DeviceMemory::from_raw(memory), None);
            }
        }
    }
}

/// Pick only the physical device whose Vulkan LUID matches the encoder adapter.
///
/// # Safety
/// instance must be a live Vulkan instance created with properties2 enabled.
pub unsafe fn physical_device_for_luid(instance: usize, target_luid: [u8; 8]) -> Option<usize> {
    let entry = unsafe { Entry::load().ok()? };
    let instance = vk::Instance::from_raw(instance as u64);
    let dispatch = unsafe { ash::Instance::load(entry.static_fn(), instance) };
    let props2 = ash::khr::get_physical_device_properties2::Instance::new(&entry, &dispatch);
    for device in unsafe { dispatch.enumerate_physical_devices().ok()? } {
        let mut id = vk::PhysicalDeviceIDProperties::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
        unsafe {
            (props2.fp().get_physical_device_properties2_khr)(device, &mut properties);
        }
        if id.device_luid_valid != 0 && id.device_luid == target_luid {
            return Some(device.as_raw() as usize);
        }
    }
    None
}

pub fn default_adapter_luid_bytes() -> Option<[u8; 8]> {
    let (low, high) = crate::d3d11::default_adapter_luid()?;
    let mut bytes = [0u8; 8];
    bytes[0..4].copy_from_slice(&low.to_ne_bytes());
    bytes[4..8].copy_from_slice(&high.to_ne_bytes());
    Some(bytes)
}

pub const REQUIRED_INSTANCE_EXTENSIONS: &[&CStr] = &[
    c"VK_KHR_external_memory_capabilities",
    c"VK_KHR_get_physical_device_properties2",
];
pub const REQUIRED_DEVICE_EXTENSIONS: &[&CStr] = &[
    c"VK_KHR_external_memory",
    c"VK_KHR_external_memory_win32",
    c"VK_KHR_dedicated_allocation",
    c"VK_KHR_get_memory_requirements2",
    c"VK_KHR_image_format_list",
    c"VK_KHR_win32_keyed_mutex",
];

type PfnGetInstanceProcAddr = vk::PFN_vkGetInstanceProcAddr;
static CREATED_INSTANCE: AtomicUsize = AtomicUsize::new(0);

unsafe fn merge_extensions(
    existing: *const *const std::ffi::c_char,
    count: u32,
    required: &[&CStr],
) -> Vec<*const std::ffi::c_char> {
    let mut names = Vec::with_capacity(count as usize + required.len());
    let mut seen = Vec::<Vec<u8>>::new();
    for index in 0..count as usize {
        let name = unsafe { *existing.add(index) };
        if !name.is_null() {
            let bytes = unsafe { CStr::from_ptr(name) }.to_bytes().to_vec();
            if !seen.iter().any(|seen_name| *seen_name == bytes) {
                seen.push(bytes);
                names.push(name);
            }
        }
    }
    for required_name in required {
        if !seen.iter().any(|name| *name == required_name.to_bytes()) {
            seen.push(required_name.to_bytes().to_vec());
            names.push(required_name.as_ptr());
        }
    }
    names
}

/// Implements xrCreateVulkanInstanceKHR while preserving the application's
/// create info, allocator, pNext chain, layers and dispatch function.
///
/// # Safety
/// All pointers must follow the OpenXR Vulkan enable2 contract.
pub unsafe fn create_instance(
    gipa: usize,
    app_info: *const vk::InstanceCreateInfo<'_>,
    allocator: *const vk::AllocationCallbacks<'_>,
    out_instance: *mut usize,
    out_result: *mut i32,
) -> bool {
    let gipa: PfnGetInstanceProcAddr = unsafe { std::mem::transmute(gipa) };
    let Some(create_proc) = (unsafe { gipa(vk::Instance::null(), c"vkCreateInstance".as_ptr()) })
    else {
        unsafe { *out_result = vk::Result::ERROR_INITIALIZATION_FAILED.as_raw() };
        return false;
    };
    let create: vk::PFN_vkCreateInstance = unsafe { std::mem::transmute(create_proc) };
    let app_info = unsafe { &*app_info };
    let extensions = unsafe {
        merge_extensions(
            app_info.pp_enabled_extension_names,
            app_info.enabled_extension_count,
            REQUIRED_INSTANCE_EXTENSIONS,
        )
    };
    let mut merged_info = *app_info;
    merged_info.enabled_extension_count = extensions.len() as u32;
    merged_info.pp_enabled_extension_names = extensions.as_ptr();
    let mut instance = vk::Instance::null();
    let result = unsafe { create(&merged_info, allocator, &mut instance) };
    unsafe {
        *out_result = result.as_raw();
        *out_instance = instance.as_raw() as usize;
    }
    if result == vk::Result::SUCCESS {
        CREATED_INSTANCE.store(instance.as_raw() as usize, Ordering::Release);
    }
    result == vk::Result::SUCCESS
}

/// Implements xrCreateVulkanDeviceKHR without replacing application queue,
/// feature, allocator, extension or dispatch requirements.
///
/// # Safety
/// All pointers must follow the OpenXR Vulkan enable2 contract.
pub unsafe fn create_device(
    gipa: usize,
    physical_device: usize,
    app_info: *const vk::DeviceCreateInfo<'_>,
    allocator: *const vk::AllocationCallbacks<'_>,
    out_device: *mut usize,
    out_result: *mut i32,
) -> bool {
    let gipa: PfnGetInstanceProcAddr = unsafe { std::mem::transmute(gipa) };
    let instance = vk::Instance::from_raw(CREATED_INSTANCE.load(Ordering::Acquire) as u64);
    let Some(create_proc) = (unsafe { gipa(instance, c"vkCreateDevice".as_ptr()) }) else {
        unsafe { *out_result = vk::Result::ERROR_INITIALIZATION_FAILED.as_raw() };
        return false;
    };
    let create: vk::PFN_vkCreateDevice = unsafe { std::mem::transmute(create_proc) };
    let app_info = unsafe { &*app_info };
    let extensions = unsafe {
        merge_extensions(
            app_info.pp_enabled_extension_names,
            app_info.enabled_extension_count,
            REQUIRED_DEVICE_EXTENSIONS,
        )
    };
    let mut merged_info = *app_info;
    merged_info.enabled_extension_count = extensions.len() as u32;
    merged_info.pp_enabled_extension_names = extensions.as_ptr();
    let mut device = vk::Device::null();
    let result = unsafe {
        create(
            vk::PhysicalDevice::from_raw(physical_device as u64),
            &merged_info,
            allocator,
            &mut device,
        )
    };
    unsafe {
        *out_result = result.as_raw();
        *out_device = device.as_raw() as usize;
    }
    result == vk::Result::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize};

    static TEST_CREATE_EXTENSION_COUNT: AtomicU32 = AtomicU32::new(0);
    static TEST_CREATE_ALLOCATOR: AtomicUsize = AtomicUsize::new(0);
    static TEST_CREATE_CHAIN: AtomicUsize = AtomicUsize::new(0);
    static TEST_CREATE_APP_EXTENSION: AtomicBool = AtomicBool::new(false);

    #[test]
    #[ignore = "requires a Windows Vulkan GPU; run with --ignored --test-threads=1"]
    fn cpu_fallback_array_transfer_readback_gpu_regression() {
        struct OwnedGpu {
            vk: VulkanFns,
            commands: Vec<CommandResources>,
            image: Option<(u64, u64)>,
        }

        impl Drop for OwnedGpu {
            fn drop(&mut self) {
                unsafe {
                    // Only this test owns the device and queue, including pending test work.
                    let _ = self.vk.device.queue_wait_idle(self.vk.queue);
                    self.vk.destroy_command_resources(&self.commands);
                    if let Some((image, memory)) = self.image {
                        self.vk.destroy_image(image, memory);
                    }
                    self.vk.device.destroy_device(None);
                    self.vk.instance.destroy_instance(None);
                }
            }
        }

        let entry = unsafe { Entry::load() }.expect("load Windows Vulkan loader");
        let app_info = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
        let extensions = [c"VK_KHR_get_physical_device_properties2".as_ptr()];
        let instance_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&extensions);
        let instance = unsafe { entry.create_instance(&instance_info, None) }
            .expect("create CPU fallback test instance");
        let setup = (|| -> Result<_, vk::Result> {
            let mut devices = unsafe { instance.enumerate_physical_devices()? };
            let preferred = default_adapter_luid_bytes().and_then(|luid| unsafe {
                physical_device_for_luid(instance.handle().as_raw() as usize, luid)
            });
            devices.sort_by_key(|device| Some(device.as_raw() as usize) != preferred);
            let (physical_device, queue_family_index) = devices
                .into_iter()
                .find_map(|device| {
                    unsafe { instance.get_physical_device_queue_family_properties(device) }
                        .iter()
                        .position(|family| {
                            family.queue_count != 0
                                && family.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                        })
                        .map(|index| (device, index as u32))
                })
                .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
            let priorities = [1.0];
            let queues = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family_index)
                .queue_priorities(&priorities)];
            // CPU fallback needs no external-memory or keyed-mutex device extensions.
            let device_info = vk::DeviceCreateInfo::default().queue_create_infos(&queues);
            let device = unsafe { instance.create_device(physical_device, &device_info, None)? };
            Ok((physical_device, queue_family_index, device))
        })();
        let (physical_device, queue_family_index, device) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                unsafe { instance.destroy_instance(None) };
                panic!("create CPU fallback test device: {error:?}");
            }
        };
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };
        let mut gpu = OwnedGpu {
            vk: VulkanFns {
                _entry: entry,
                instance,
                device,
                physical_device,
                queue,
                queue_family_index,
            },
            commands: Vec::new(),
            image: None,
        };
        // A recreated XR session must reload its own dispatch context, while
        // repeated lookups in one session must retain the same context.
        let mut previous_dispatch = None;
        for _ in 0..2 {
            *crate::state::SESSION.lock() = crate::state::SessionState::default();
            *crate::state::GRAPHICS.lock() = crate::state::GraphicsState {
                api: crate::state::GraphicsApi::Vulkan,
                vk_instance: gpu.vk.instance.handle().as_raw() as usize,
                vk_physical_device: physical_device.as_raw() as usize,
                vk_device: gpu.vk.device.handle().as_raw() as usize,
                vk_queue_family_index: queue_family_index,
                vk_queue_index: 0,
                ..Default::default()
            };
            let info = openxr_sys::SessionCreateInfo {
                ty: openxr_sys::StructureType::SESSION_CREATE_INFO,
                next: std::ptr::null(),
                create_flags: Default::default(),
                system_id: openxr_sys::SystemId::from_raw(1),
            };
            let mut session = openxr_sys::Session::from_raw(0);
            assert_eq!(
                unsafe {
                    crate::runtime::create_session(
                        openxr_sys::Instance::from_raw(1),
                        &info,
                        &mut session,
                    )
                },
                openxr_sys::Result::SUCCESS
            );
            let cached = crate::state::GRAPHICS.lock().vulkan.clone().unwrap();
            for _ in 0..100 {
                assert!(std::sync::Arc::ptr_eq(
                    &cached,
                    crate::state::GRAPHICS.lock().vulkan.as_ref().unwrap()
                ));
            }
            if let Some(previous) = previous_dispatch {
                assert!(!std::sync::Arc::ptr_eq(&cached, &previous));
            }
            assert_eq!(
                unsafe { crate::runtime::destroy_session(session) },
                openxr_sys::Result::SUCCESS
            );
            assert!(crate::state::GRAPHICS.lock().vulkan.is_none());
            previous_dispatch = Some(cached);
        }
        drop(previous_dispatch);
        gpu.commands = gpu.vk.create_command_resources(2).expect("test commands");
        const WIDTH: u32 = 17;
        const HEIGHT: u32 = 13;
        gpu.image = Some(
            unsafe {
                gpu.vk.create_owned_image(
                    VK_FORMAT_R8G8B8A8_SRGB,
                    WIDTH,
                    HEIGHT,
                    2,
                    vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::COLOR_ATTACHMENT,
                )
            }
            .expect("owned two-layer fallback image"),
        );
        let (image, memory) = gpu.image.unwrap();
        let runtime_command = gpu.commands[0];
        let app_command = gpu.commands[1];
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(2);
        let colors = [
            [[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0]],
            [[0.0, 0.0, 1.0, 1.0], [1.0, 1.0, 0.0, 1.0]],
        ];
        let expected: [[[u8; 4]; 2]; 2] = [
            [[255, 0, 0, 255], [0, 255, 0, 255]],
            [[0, 0, 255, 255], [255, 255, 0, 255]],
        ];
        for (cycle, colors) in colors.into_iter().enumerate() {
            unsafe {
                gpu.vk
                    .acquire_image(
                        image,
                        memory,
                        2,
                        runtime_command,
                        false,
                        cycle == 0,
                        cycle != 0,
                    )
                    .expect("acquire fallback image");
                gpu.vk
                    .wait_fence(runtime_command.fence, u64::MAX)
                    .expect("wait fallback image");
                let pool = vk::CommandPool::from_raw(app_command.pool);
                let cmd = vk::CommandBuffer::from_raw(app_command.command_buffer as u64);
                gpu.vk
                    .device
                    .reset_command_pool(pool, vk::CommandPoolResetFlags::empty())
                    .expect("reset transfer pool");
                gpu.vk
                    .device
                    .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                    .expect("begin transfer fill");
                let to_transfer = vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(vk::Image::from_raw(image))
                    .subresource_range(range);
                gpu.vk.device.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_transfer],
                );
                for (layer, color) in colors.into_iter().enumerate() {
                    gpu.vk.device.cmd_clear_color_image(
                        cmd,
                        vk::Image::from_raw(image),
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &vk::ClearColorValue { float32: color },
                        &[range.base_array_layer(layer as u32).layer_count(1)],
                    );
                }
                let to_attachment = vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(vk::Image::from_raw(image))
                    .subresource_range(range);
                gpu.vk.device.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_attachment],
                );
                gpu.vk.device.end_command_buffer(cmd).expect("end fill");
                let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
                gpu.vk
                    .device
                    .queue_submit(queue, &[submit], vk::Fence::null())
                    .expect("submit transfer fill");
            }
            for pass in 0..2 {
                unsafe {
                    gpu.vk
                        .release_image(image, memory, 2, runtime_command, false, true)
                        .expect("release fallback image");
                }
                for layer in [0, 1] {
                    let pixels = unsafe {
                        gpu.vk
                            .readback_image(image, WIDTH, HEIGHT, layer, runtime_command, true)
                    }
                    .expect("read fallback array layer");
                    assert_eq!(pixels.len(), (WIDTH * HEIGHT * 4) as usize);
                    assert!(
                        pixels
                            .chunks_exact(4)
                            .all(|pixel| pixel == expected[cycle][layer as usize]),
                        "cycle {cycle}, pass {pass}, layer {layer}: transfer-written pixels changed"
                    );
                }
                if pass == 0 {
                    // A repeated acquire/release must preserve both layers and the attachment layout.
                    unsafe {
                        gpu.vk
                            .acquire_image(image, memory, 2, runtime_command, false, false, true)
                            .expect("reacquire fallback image");
                        gpu.vk
                            .wait_fence(runtime_command.fence, u64::MAX)
                            .expect("wait reacquire");
                    }
                }
            }
        }
    }

    #[test]
    fn extension_requirements_include_interop_and_format_support() {
        assert!(REQUIRED_INSTANCE_EXTENSIONS
            .iter()
            .any(|ext| *ext == c"VK_KHR_get_physical_device_properties2"));
        for name in [
            c"VK_KHR_external_memory_win32",
            c"VK_KHR_image_format_list",
            c"VK_KHR_win32_keyed_mutex",
        ] {
            assert!(REQUIRED_DEVICE_EXTENSIONS.iter().any(|ext| *ext == name));
        }
    }

    #[test]
    fn extension_merge_keeps_app_names_and_deduplicates() {
        let app_names = [
            c"VK_EXT_custom".as_ptr(),
            c"VK_KHR_external_memory".as_ptr(),
        ];
        let merged = unsafe {
            merge_extensions(
                app_names.as_ptr(),
                app_names.len() as u32,
                &[c"VK_KHR_external_memory", c"VK_KHR_external_memory_win32"],
            )
        };
        let names = merged
            .iter()
            .map(|name| unsafe { CStr::from_ptr(*name) }.to_bytes())
            .collect::<Vec<_>>();
        assert_eq!(names[0], b"VK_EXT_custom");
        assert_eq!(
            names
                .iter()
                .filter(|name| **name == b"VK_KHR_external_memory")
                .count(),
            1
        );
        assert!(names
            .iter()
            .any(|name| *name == b"VK_KHR_external_memory_win32"));
    }

    #[test]
    fn supported_swapchain_formats_map_to_typed_dxgi() {
        for (vk, dxgi) in [
            (VK_FORMAT_R8G8B8A8_UNORM, 28),
            (VK_FORMAT_R8G8B8A8_SRGB, 29),
            (VK_FORMAT_B8G8R8A8_UNORM, 87),
            (VK_FORMAT_B8G8R8A8_SRGB, 91),
        ] {
            assert!(is_supported_format(vk));
            assert_eq!(vk_format_to_dxgi_typed(vk), dxgi);
        }
        assert!(!is_supported_format(97));
        assert_eq!(vk_format_to_dxgi(97), 0);
    }

    unsafe extern "system" fn test_vk_create_instance(
        create_info: *const vk::InstanceCreateInfo<'_>,
        allocator: *const vk::AllocationCallbacks<'_>,
        instance: *mut vk::Instance,
    ) -> vk::Result {
        let info = unsafe { &*create_info };
        TEST_CREATE_EXTENSION_COUNT.store(info.enabled_extension_count, Ordering::Relaxed);
        TEST_CREATE_ALLOCATOR.store(allocator as usize, Ordering::Relaxed);
        TEST_CREATE_CHAIN.store(info.p_next as usize, Ordering::Relaxed);
        let names = unsafe {
            std::slice::from_raw_parts(
                info.pp_enabled_extension_names,
                info.enabled_extension_count as usize,
            )
        };
        TEST_CREATE_APP_EXTENSION.store(
            names
                .iter()
                .any(|name| unsafe { CStr::from_ptr(*name) } == c"VK_EXT_test"),
            Ordering::Relaxed,
        );
        unsafe { *instance = vk::Instance::null() };
        vk::Result::ERROR_OUT_OF_HOST_MEMORY
    }

    unsafe extern "system" fn test_vk_get_instance_proc_addr(
        _instance: vk::Instance,
        name: *const std::ffi::c_char,
    ) -> vk::PFN_vkVoidFunction {
        if !name.is_null() && unsafe { CStr::from_ptr(name) } == c"vkCreateInstance" {
            Some(unsafe {
                std::mem::transmute::<vk::PFN_vkCreateInstance, unsafe extern "system" fn()>(
                    test_vk_create_instance,
                )
            })
        } else {
            None
        }
    }

    #[test]
    fn create_instance_preserves_dispatch_and_allocator_and_propagates_vk_error() {
        let extensions = [c"VK_EXT_test".as_ptr()];
        let marker = 0x1234u32;
        let mut create_info =
            vk::InstanceCreateInfo::default().enabled_extension_names(&extensions);
        create_info.p_next = (&marker as *const u32).cast();
        let allocator = vk::AllocationCallbacks::default();
        let mut instance = 99usize;
        let mut result = vk::Result::SUCCESS.as_raw();
        let succeeded = unsafe {
            create_instance(
                test_vk_get_instance_proc_addr as *const () as usize,
                &create_info,
                &allocator,
                &mut instance,
                &mut result,
            )
        };
        assert!(!succeeded);
        assert_eq!(result, vk::Result::ERROR_OUT_OF_HOST_MEMORY.as_raw());
        assert_eq!(instance, 0);
        assert_eq!(
            TEST_CREATE_EXTENSION_COUNT.load(Ordering::Relaxed),
            1 + REQUIRED_INSTANCE_EXTENSIONS.len() as u32
        );
        assert_eq!(
            TEST_CREATE_ALLOCATOR.load(Ordering::Relaxed),
            &allocator as *const _ as usize
        );
        assert_eq!(
            TEST_CREATE_CHAIN.load(Ordering::Relaxed),
            create_info.p_next as usize
        );
        assert!(TEST_CREATE_APP_EXTENSION.load(Ordering::Relaxed));
    }
}
