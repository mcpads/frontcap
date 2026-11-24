// Simulator experiment: configure the existing platform attachment before the app's main.
// No consumer source, testing entry point, or SDK is required.
#include <dlfcn.h>
#include <limits.h>
#include <mach-o/dyld.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

__attribute__((constructor)) static void attach(void) {
    char executable[PATH_MAX], path[PATH_MAX];
    uint32_t capacity = sizeof(executable);
    if (_NSGetExecutablePath(executable, &capacity)) _exit(120);
    char *slash = strrchr(executable, '/');
    if (!slash) _exit(121);
    *slash = '\0';
    int n = snprintf(path, sizeof(path), "%s/FrontcapAttachment/timeline.js", executable);
    if (n < 0 || (size_t)n >= sizeof(path)) _exit(122);
    if (setenv("FRONTCAP_WEBVIEW_SCRIPT", path, 1) ||
        setenv("FRONTCAP_WEBVIEW_CONTROL", "pages", 1) ||
        setenv("FRONTCAP_MUTE_AUDIO", "1", 1)) _exit(123);
    n = snprintf(path, sizeof(path), "%s/FrontcapAttachment/native-input.dylib", executable);
    if (n < 0 || (size_t)n >= sizeof(path)) _exit(124);
    if (!dlopen(path, RTLD_NOW | RTLD_LOCAL)) {
        fprintf(stderr, "frontcap attachment failed: %s\n", dlerror());
        _exit(125);
    }
}
