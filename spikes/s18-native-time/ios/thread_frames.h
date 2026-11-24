// Best-effort raw execution frames for failure diagnostics. No thread suspension or symbol lookup.
// The caller owns the output and supplies the scheduler observation's shared real-time budget.
#include <mach/vm_map.h>
#include <mach-o/dyld.h>
#include <mach-o/loader.h>

static int frame_budget_available(uint64_t started, mach_timebase_info_data_t timebase) {
  return (__uint128_t)(fc_real_abs() - started) * timebase.numer / timebase.denom < 20000000;
}

static void trace_thread_frames(int fd, thread_t thread, uint64_t id, uint64_t started,
                                mach_timebase_info_data_t timebase) {
  uint64_t begin = fc_real_abs();
  if (!frame_budget_available(started, timebase)) {
    dprintf(fd, "thread_stack id=%llu stop=budget host_ticks=%llu\n", id, begin);
    return;
  }
#if defined(__arm64__)
  arm_thread_state64_t state = {0};
  mach_msg_type_number_t count = ARM_THREAD_STATE64_COUNT;
  kern_return_t result = thread_get_state(thread, ARM_THREAD_STATE64, (thread_state_t)&state, &count);
  dprintf(fd, "thread_stack id=%llu state_result=%d host_ticks=%llu pc=0x%llx lr=0x%llx\n",
    id, result, begin, (uint64_t)arm_thread_state64_get_pc(state),
    (uint64_t)arm_thread_state64_get_lr(state));
  const char *stop = "state_error";
  unsigned captured = 0;
  if (result == KERN_SUCCESS) {
    uint64_t fp = arm_thread_state64_get_fp(state);
    stop = "depth_limit";
    for (unsigned depth = 0; depth < 12; depth++) {
      if (!fp) { stop = "null_frame"; break; }
      if (fp % sizeof(uint64_t)) { stop = "unaligned_frame"; break; }
      if (!frame_budget_available(started, timebase)) { stop = "budget"; break; }
      uint64_t frame[2] = {0}; vm_size_t size = 0;
      result = vm_read_overwrite(mach_task_self(), (vm_address_t)fp, sizeof(frame),
                                (vm_address_t)frame, &size);
      if (result != KERN_SUCCESS || size != sizeof(frame)) { stop = "read_error"; break; }
      dprintf(fd, "thread_frame id=%llu depth=%u fp=0x%llx return=0x%llx\n",
        id, depth, fp, frame[1]);
      captured++;
      if (frame[0] <= fp) { stop = "nonascending_frame"; break; }
      if (frame[0] - fp > 1048576) { stop = "distant_frame"; break; }
      fp = frame[0];
    }
  }
  dprintf(fd, "thread_stack_end id=%llu frames=%u stop=%s result=%d host_ticks=%llu\n",
    id, captured, stop, result, fc_real_abs());
#else
  dprintf(fd, "thread_stack id=%llu stop=unsupported_arch host_ticks=%llu\n", id, begin);
#endif
}

// UUID + loaded header address identify the binary and its ASLR load address for offline atos.
// Read headers through VM APIs: an image may unload during this non-atomic inventory.
static void trace_frame_images(int fd, uint64_t started, mach_timebase_info_data_t timebase) {
  uint32_t total = _dyld_image_count(), recorded = 0;
  for (uint32_t i = 0; i < total && i < 512 && frame_budget_available(started, timebase); i++) {
    const struct mach_header *address = _dyld_get_image_header(i);
    struct mach_header_64 header = {0}; vm_size_t size = 0;
    kern_return_t result = vm_read_overwrite(mach_task_self(), (vm_address_t)address,
      sizeof(header), (vm_address_t)&header, &size);
    const char *stop = "header_error";
    char uuid[33] = "";
    if (result == KERN_SUCCESS && size == sizeof(header) && header.magic == MH_MAGIC_64 &&
        header.sizeofcmds <= 1048576) {
      uint64_t offset = 0; stop = "uuid_missing";
      for (uint32_t j = 0; j < header.ncmds && j < 256; j++) {
        if (!frame_budget_available(started, timebase)) { stop = "budget"; break; }
        if (offset + sizeof(struct load_command) > header.sizeofcmds) { stop = "invalid_commands"; break; }
        struct load_command command = {0};
        vm_address_t at = (vm_address_t)address + sizeof(header) + offset;
        result = vm_read_overwrite(mach_task_self(), at, sizeof(command), (vm_address_t)&command, &size);
        if (result != KERN_SUCCESS || size != sizeof(command)) { stop = "read_error"; break; }
        if (command.cmdsize < sizeof(command) || offset + command.cmdsize > header.sizeofcmds) {
          stop = "invalid_commands"; break;
        }
        if (command.cmd == LC_UUID) {
          struct uuid_command value = {0};
          if (command.cmdsize < sizeof(value)) { stop = "invalid_uuid"; break; }
          result = vm_read_overwrite(mach_task_self(), at, sizeof(value), (vm_address_t)&value, &size);
          if (result != KERN_SUCCESS || size != sizeof(value)) { stop = "read_error"; break; }
          for (unsigned k = 0; k < 16; k++) snprintf(uuid + k * 2, 3, "%02x", value.uuid[k]);
          stop = "uuid"; break;
        }
        offset += command.cmdsize;
        if (j == 255) stop = "command_limit";
      }
    }
    dprintf(fd, "stack_image index=%u header=%p uuid=%s stop=%s result=%d\n",
      i, address, uuid[0] ? uuid : "unavailable", stop, result);
    recorded++;
  }
  dprintf(fd, "stack_images_end captured=%u omitted=%u host_ticks=%llu\n",
    recorded, total - recorded, fc_real_abs());
}
