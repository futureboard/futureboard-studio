#include <assert.h>
#include <stdio.h>

#include "mkff/mkff.h"

#if defined(_WIN32)
#define WIN32_LEAN_AND_MEAN
#include <windows.h>

/* Windows has no /proc/self/fd; GetProcessHandleCount() (covering
 * every kernel object handle, not just files) is the closest analogue
 * for "did repeated create/destroy leak an OS resource". */
static int count_open_fds(void) {
    DWORD count = 0;
    if (!GetProcessHandleCount(GetCurrentProcess(), &count)) {
        return -1;
    }
    return (int)count;
}
#else
#include <dirent.h>

static int count_open_fds(void) {
    DIR *d = opendir("/proc/self/fd");
    if (!d) return -1;
    int count = 0;
    struct dirent *entry;
    while ((entry = readdir(d)) != NULL) {
        if (entry->d_name[0] != '.') count++;
    }
    closedir(d);
    return count;
}
#endif

static int g_saw_log = 0;

static void log_cb(void *user_data, MKFF_LogLevel level, const char *component, const char *message) {
    (void)level;
    (void)component;
    (void)message;
    int *flag = (int *)user_data;
    *flag = 1;
}

int main(void) {
    /* Default-constructed desc (NULL) must work. */
    MKFF_Context *ctx1 = NULL;
    MKFF_Result r = mkff_context_create(NULL, &ctx1);
    assert(r == MKFF_RESULT_OK);
    assert(ctx1 != NULL);
    mkff_context_destroy(ctx1);

    /* Explicit desc with logging. */
    MKFF_ContextDesc desc;
    MKFF_INIT_STRUCT_HEADER(&desc);
    desc.log_callback = log_cb;
    desc.log_user_data = &g_saw_log;
    desc.log_min_level = MKFF_LOG_LEVEL_TRACE;

    MKFF_Context *ctx2 = NULL;
    r = mkff_context_create(&desc, &ctx2);
    assert(r == MKFF_RESULT_OK);
    assert(ctx2 != NULL);

    /* get_last_error must never return NULL. */
    const char *err = mkff_context_get_last_error(ctx2);
    assert(err != NULL);

    mkff_context_destroy(ctx2);

    /* Repeated create/destroy must not crash or leak platform module
     * handles (acceptance criterion: "repeated create/destroy does not
     * leak VA objects or file descriptors"). Warm up once first so the
     * dynamic linker's own lazily-opened fds (ld.so.cache, the platform
     * module the very first time) don't show up as a false-positive
     * "leak" in the delta below. */
    {
        MKFF_Context *warmup = NULL;
        assert(mkff_context_create(NULL, &warmup) == MKFF_RESULT_OK);
        mkff_context_destroy(warmup);
    }

    int fds_before = count_open_fds();
    for (int i = 0; i < 200; i++) {
        MKFF_Context *ctx = NULL;
        assert(mkff_context_create(NULL, &ctx) == MKFF_RESULT_OK);
        mkff_context_destroy(ctx);
    }
    int fds_after = count_open_fds();
    if (fds_before >= 0 && fds_after >= 0) {
        assert(fds_after == fds_before);
    }

    /* mkff_context_destroy(NULL) must be a safe no-op. */
    mkff_context_destroy(NULL);

    printf("test_context: OK\n");
    return 0;
}
