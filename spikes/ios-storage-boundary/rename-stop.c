// Opt-in diagnostic interposition. The external owner must resume or retire this process.
#include <errno.h>
#ifndef __APPLE__
#include <dlfcn.h>
#endif
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#ifdef FC_RENAME_RELATIVE_SOURCE
// Prepared Simulator products resolve frozen relative paths in their own data container.
__attribute__((constructor)) static void configure_boundary(void) {
  const char *home = getenv("HOME");
  const char *relative[] = {FC_RENAME_RELATIVE_SOURCE, FC_RENAME_RELATIVE_DESTINATION, FC_RENAME_RELATIVE_MARKER};
  const char *keys[] = {"FC_RENAME_SOURCE", "FC_RENAME_DESTINATION", "FC_RENAME_MARKER"};
  if (!home || home[0] != '/') _exit(123);
  for (size_t i = 0; i < 3; i++) {
    char path[PATH_MAX];
    int count = snprintf(path, sizeof(path), "%s/%s", home, relative[i]);
    if (count < 0 || (size_t)count >= sizeof(path) || setenv(keys[i], path, 1)) _exit(124);
  }
}
#endif

static atomic_bool reached;
static void stop_before_rename(const char *source, const char *destination) {
  const char *expected_source = getenv("FC_RENAME_SOURCE");
  const char *expected_destination = getenv("FC_RENAME_DESTINATION");
  const char *marker = getenv("FC_RENAME_MARKER");
  if (source && destination && expected_source && expected_destination && marker &&
      strcmp(source, expected_source) == 0 && strcmp(destination, expected_destination) == 0 &&
      !atomic_exchange(&reached, 1)) {
    // Never replace an existing receipt. An invalid diagnostic cannot continue the mutation.
    int descriptor = open(marker, O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (descriptor < 0) _exit(120);
    char receipt[128];
    int size = snprintf(receipt, sizeof(receipt), "{\"stage\":\"before_rename\",\"pid\":%ld}\n", (long)getpid());
    if (size <= 0 || (size_t)size >= sizeof(receipt) ||
        write(descriptor, receipt, (size_t)size) != size || fsync(descriptor) != 0 || close(descriptor) != 0)
      _exit(121);
    if (raise(SIGSTOP) != 0) _exit(122);
  }
}

static int observed_rename(const char *source, const char *destination) {
  stop_before_rename(source, destination);
#ifdef __APPLE__
  // dyld does not interpose references from the replacement's own image.
  return rename(source, destination);
#else
  int (*original)(const char *, const char *) = dlsym(RTLD_NEXT, "rename");
  if (!original) { errno = ENOSYS; return -1; }
  return original(source, destination);
#endif
}
#ifdef __APPLE__
__attribute__((used, section("__DATA,__interpose")))
static struct { const void *replacement; const void *original; } interposition = {
  (const void *)&observed_rename, (const void *)&rename
};

#else
int rename(const char *source, const char *destination) {
  return observed_rename(source, destination);
}

int renameat(int source_directory, const char *source,
             int destination_directory, const char *destination) {
  // Absolute paths ignore directory descriptors. Do not guess relative-path identity.
  if (source && destination && source[0] == '/' && destination[0] == '/')
    stop_before_rename(source, destination);
  int (*original)(int, const char *, int, const char *) = dlsym(RTLD_NEXT, "renameat");
  if (!original) { errno = ENOSYS; return -1; }
  return original(source_directory, source, destination_directory, destination);
}
#endif
