#include <stdio.h>
#include <string.h>

#if defined(MKFF_CLI_HAVE_LINUX_COMMANDS)
int cmd_devices(int argc, char **argv);
int cmd_va_info(int argc, char **argv);
int cmd_export_test(int argc, char **argv);
#endif
int cmd_decode_test(int argc, char **argv);
int cmd_benchmark(int argc, char **argv);
int cmd_codec_info(int argc, char **argv);

static void print_usage(const char *prog) {
    fprintf(stderr,
            "usage: %s <command> [args]\n"
            "\n"
            "commands:\n"
#if defined(MKFF_CLI_HAVE_LINUX_COMMANDS)
            "  devices                          list DRM render nodes\n"
            "  va-info                           report VA-API vendor/driver and supported profiles\n"
#endif
            "  decode-test <input> [--codec h264|hevc] [--frames N] [--backend auto|hw|sw]\n"
#if defined(MKFF_CLI_HAVE_LINUX_COMMANDS)
            "  export-test <input.h264> [--frames N]   decode and export decoded surfaces as dma-buf\n"
#endif
            "  benchmark <input> [--codec h264|hevc] [--seconds N]\n"
            "  codec-info <h264|hevc> [--backend auto|hw|sw]\n",
            prog);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        print_usage(argv[0]);
        return 2;
    }

    const char *command = argv[1];
    int sub_argc = argc - 1;
    char **sub_argv = argv + 1;

#if defined(MKFF_CLI_HAVE_LINUX_COMMANDS)
    if (strcmp(command, "devices") == 0) {
        return cmd_devices(sub_argc, sub_argv);
    } else if (strcmp(command, "va-info") == 0) {
        return cmd_va_info(sub_argc, sub_argv);
    } else if (strcmp(command, "export-test") == 0) {
        return cmd_export_test(sub_argc, sub_argv);
    }
#endif
    if (strcmp(command, "decode-test") == 0) {
        return cmd_decode_test(sub_argc, sub_argv);
    } else if (strcmp(command, "benchmark") == 0) {
        return cmd_benchmark(sub_argc, sub_argv);
    } else if (strcmp(command, "codec-info") == 0) {
        return cmd_codec_info(sub_argc, sub_argv);
    } else if (strcmp(command, "-h") == 0 || strcmp(command, "--help") == 0) {
        print_usage(argv[0]);
        return 0;
    }

    fprintf(stderr, "unknown command: %s\n\n", command);
    print_usage(argv[0]);
    return 2;
}
