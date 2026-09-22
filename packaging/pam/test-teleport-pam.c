/* Unit fixtures: no real PAM authentication, NSS lookup or account changes.
 * cc -O2 -Wall -Wextra -Werror -o test-pam test-teleport-pam.c && ./test-pam
 * Requires PAM development headers, but does NOT link libpam.
 */
#define main worker_main
#define pam_start fixture_start
#define pam_authenticate fixture_authenticate
#define pam_acct_mgmt fixture_account
#define pam_get_item fixture_item
#define pam_end fixture_end
#define getpwnam_r fixture_user
#define getuid fixture_uid
#define geteuid fixture_euid
#define getgid fixture_gid
#define getegid fixture_egid
#define stat fixture_stat
#include "teleport-pam.c"
#undef main
#include <assert.h>
#include <sys/wait.h>

static uid_t test_uid = 1000, test_euid = 1000;
static int account_result = PAM_SUCCESS, auth_result = PAM_SUCCESS;
static int remap = 0, repeated = 0;
static int policy_missing = 0;
static mode_t policy_mode = S_IFREG | 0644;
static int message_style = PAM_PROMPT_ECHO_OFF;
static const char *prompt = "Password: ";
static const struct pam_conv *active_conversation;

uid_t fixture_uid(void) { return test_uid; }
uid_t fixture_euid(void) { return test_euid; }
gid_t fixture_gid(void) { return 1000; }
gid_t fixture_egid(void) { return 1000; }
int fixture_stat(const char *path, struct stat *entry) {
    assert(!strcmp(path, "/etc/pam.d/teleport"));
    if (policy_missing) return -1;
    memset(entry, 0, sizeof(*entry));
    entry->st_mode = policy_mode;
    return 0;
}
int fixture_user(const char *name, struct passwd *entry, char *buffer,
                 size_t length, struct passwd **found) {
    (void)buffer; (void)length;
    *found = NULL;
    if (strcmp(name, "fixture") && strcmp(name, "other")) return 0;
    entry->pw_name = (char *)name;
    entry->pw_uid = strcmp(name, "fixture") ? 1001 : 1000;
    *found = entry;
    return 0;
}
int fixture_start(const char *service, const char *user, const struct pam_conv *conv,
                  pam_handle_t **handle) {
    assert(!strcmp(service, "teleport") && !strcmp(user, "fixture"));
    active_conversation = conv;
    *handle = (pam_handle_t *)conv;
    return PAM_SUCCESS;
}
int fixture_authenticate(pam_handle_t *handle, int flags) {
    (void)handle;
    assert(flags == PAM_DISALLOW_NULL_AUTHTOK);
    struct pam_message message = {message_style, prompt};
    const struct pam_message *messages[] = {&message};
    for (int i = 0; i < (repeated ? 2 : 1); ++i) {
        struct pam_response *response = NULL;
        int result = active_conversation->conv(1, messages, &response,
                                               active_conversation->appdata_ptr);
        if (result != PAM_SUCCESS) return result;
        assert(response && !strcmp(response[0].resp, "fake-password"));
        explicit_bzero(response[0].resp, strlen(response[0].resp));
        free(response[0].resp); free(response);
    }
    return auth_result;
}
int fixture_account(pam_handle_t *handle, int flags) {
    (void)handle;
    assert(flags == PAM_DISALLOW_NULL_AUTHTOK);
    return account_result;
}
int fixture_item(const pam_handle_t *handle, int item, const void **value) {
    (void)handle;
    assert(item == PAM_USER);
    *value = remap ? "other" : "fixture";
    return PAM_SUCCESS;
}
int fixture_end(pam_handle_t *handle, int status) {
    (void)handle; (void)status;
    return PAM_SUCCESS;
}

static void run(const unsigned char *frame, size_t length, int expected_success) {
    int input[2], output[2];
    assert(pipe(input) == 0 && pipe(output) == 0);
    pid_t pid = fork();
    assert(pid >= 0);
    if (pid == 0) {
        close(input[1]); close(output[0]);
        assert(dup2(input[0], STDIN_FILENO) >= 0 && dup2(output[1], STDOUT_FILENO) >= 0);
        close(input[0]); close(output[1]);
        _exit(worker_main(1, NULL));
    }
    close(input[0]); close(output[1]);
    assert(write(input[1], frame, length) == (ssize_t)length);
    close(input[1]);
    char result[4] = {0};
    ssize_t n = read(output[0], result, sizeof(result));
    close(output[0]);
    int status;
    assert(waitpid(pid, &status, 0) == pid && WIFEXITED(status));
    assert((WEXITSTATUS(status) == 0) == expected_success);
    assert(expected_success ? (n == 3 && !strcmp(result, "OK\n")) : n == 0);
}

int main(void) {
    const unsigned char good[] = "TPAM0001\0\0\0\x07\0\0\0\x0d" "fixturefake-password";
    run(good, sizeof(good) - 1, 1);
    policy_missing = 1; run(good, sizeof(good) - 1, 0); policy_missing = 0;
    policy_mode = S_IFREG | 0666; run(good, sizeof(good) - 1, 0); policy_mode = S_IFREG | 0644;
    auth_result = PAM_AUTH_ERR; run(good, sizeof(good) - 1, 0); auth_result = PAM_SUCCESS;
    account_result = PAM_ACCT_EXPIRED; run(good, sizeof(good) - 1, 0);
    account_result = PAM_NEW_AUTHTOK_REQD; run(good, sizeof(good) - 1, 0);
    account_result = PAM_PERM_DENIED; run(good, sizeof(good) - 1, 0);
    account_result = PAM_SUCCESS;
    remap = 1; run(good, sizeof(good) - 1, 0); remap = 0;
    repeated = 1; run(good, sizeof(good) - 1, 0); repeated = 0;
    prompt = "Verification code: "; run(good, sizeof(good) - 1, 0); prompt = "Password: ";
    message_style = PAM_PROMPT_ECHO_ON; run(good, sizeof(good) - 1, 0); message_style = PAM_PROMPT_ECHO_OFF;
    test_uid = 0; test_euid = 0; run(good, sizeof(good) - 1, 0);
    test_uid = 1000; test_euid = 0; run(good, sizeof(good) - 1, 0); test_euid = 1000;
    run(good, sizeof(good), 0); /* Extra input, including trailing NUL. */
    run(good, sizeof(good) - 2, 0); /* Truncation. */
    run((const unsigned char *)"INVALID!", 8, 0);
    const unsigned char other[] = "TPAM0001\0\0\0\x05\0\0\0\x0d" "otherfake-password";
    run(other, sizeof(other) - 1, 0);
    const unsigned char blank[] = "TPAM0001\0\0\0\x07\0\0\0\0" "fixture";
    run(blank, sizeof(blank) - 1, 0);
    const unsigned char nul[] = "TPAM0001\0\0\0\x07\0\0\0\x0d" "fixturefake\0password";
    run(nul, sizeof(nul) - 1, 0);
    return 0;
}
