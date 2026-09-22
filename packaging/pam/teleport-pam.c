/* Build with the HOST distribution's compiler/libpam: cc -O2 -Wall -Wextra
 * -Werror -o teleport-pam teleport-pam.c -lpam. Never install setuid/setgid.
 * stdin: TPAM0001, two big-endian u32 lengths, username, password, EOF.
 * stdout: OK\n on success only. All failures have the same exit status.
 */
#define _GNU_SOURCE
#include <security/pam_appl.h>
#include <errno.h>
#include <pwd.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <unistd.h>

struct credentials { char *password; int supplied; };

static int read_exact(void *buffer, size_t length) {
    unsigned char *p = buffer;
    while (length) {
        ssize_t n = read(STDIN_FILENO, p, length);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return -1;
        p += n; length -= (size_t)n;
    }
    return 0;
}

static uint32_t be32(const unsigned char *p) {
    return ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) |
           ((uint32_t)p[2] << 8) | p[3];
}

static int same_user(const char *name) {
    struct passwd entry, *found = NULL;
    char buffer[65536];
    uid_t uid = getuid();
    return uid != 0 && uid == geteuid() && getgid() == getegid() &&
        getpwnam_r(name, &entry, buffer, sizeof(buffer), &found) == 0 &&
        found && found->pw_uid == uid && strcmp(name, found->pw_name) == 0;
}

static int conversation(int count, const struct pam_message **messages,
                        struct pam_response **responses, void *opaque) {
    struct credentials *credentials = opaque;
    if (count < 1 || count > 16 || !messages || !responses) return PAM_CONV_ERR;
    struct pam_response *result = calloc((size_t)count, sizeof(*result));
    if (!result) return PAM_BUF_ERR;
    for (int i = 0; i < count; ++i) {
        if (!messages[i]) goto fail;
        switch (messages[i]->msg_style) {
        case PAM_TEXT_INFO:
        case PAM_ERROR_MSG:
            break; /* Never relay module text (which may contain secrets). */
        case PAM_PROMPT_ECHO_OFF:
            /* This protocol supports a single standard password prompt only.
             * OTP, password changes, extra factors and localized challenges
             * must fail closed, not receive the account password by mistake. */
            if (credentials->supplied || !messages[i]->msg ||
                (strcmp(messages[i]->msg, "Password: ") != 0 &&
                 strcmp(messages[i]->msg, "Password:") != 0)) goto fail;
            result[i].resp = strdup(credentials->password);
            if (!result[i].resp) goto fail;
            credentials->supplied = 1;
            break;
        default:
            goto fail;
        }
    }
    *responses = result;
    return PAM_SUCCESS;
fail:
    for (int i = 0; i < count; ++i) {
        if (result[i].resp) {
            explicit_bzero(result[i].resp, strlen(result[i].resp));
            free(result[i].resp);
        }
    }
    free(result);
    return PAM_CONV_ERR;
}

int main(int argc, char **argv) {
    (void)argv;
    struct rlimit core = {0, 0};
    if (argc != 1 || setrlimit(RLIMIT_CORE, &core) != 0) return 1;
    alarm(25);
    unsigned char header[16];
    char username[257] = {0}, password[4097] = {0};
    int result = 1;
    pam_handle_t *handle = NULL;
    if (read_exact(header, sizeof(header)) || memcmp(header, "TPAM0001", 8)) goto done;
    uint32_t user_length = be32(header + 8), password_length = be32(header + 12);
    if (!user_length || user_length > 256 || !password_length || password_length > 4096)
        goto done;
    if (read_exact(username, user_length) || read_exact(password, password_length)) goto done;
    unsigned char extra;
    if (read(STDIN_FILENO, &extra, 1) != 0 ||
        memchr(username, 0, user_length) || memchr(password, 0, password_length) ||
        !same_user(username)) goto done;
    struct credentials credentials = {password, 0};
    struct pam_conv conv = {conversation, &credentials};
    /* Never fall through to the distribution's generic "other" PAM policy. */
    struct stat policy;
    if (stat("/etc/pam.d/teleport", &policy) != 0 || !S_ISREG(policy.st_mode) ||
        policy.st_uid != 0 || (policy.st_mode & 0022)) goto done;
    int status = pam_start("teleport", username, &conv, &handle);
    if (status != PAM_SUCCESS) goto done;
    status = pam_authenticate(handle, PAM_DISALLOW_NULL_AUTHTOK);
    if (status == PAM_SUCCESS && credentials.supplied)
        status = pam_acct_mgmt(handle, PAM_DISALLOW_NULL_AUTHTOK);
    else status = PAM_AUTH_ERR;
    const void *authenticated_user = NULL;
    if (status == PAM_SUCCESS &&
        pam_get_item(handle, PAM_USER, &authenticated_user) == PAM_SUCCESS &&
        authenticated_user && strcmp(username, authenticated_user) == 0 &&
        same_user(authenticated_user)) result = 0;
    if (pam_end(handle, status) != PAM_SUCCESS) result = 1;
    handle = NULL;
done:
    if (handle) pam_end(handle, PAM_ABORT);
    explicit_bzero(password, sizeof(password));
    explicit_bzero(username, sizeof(username));
    if (result == 0 && write(STDOUT_FILENO, "OK\n", 3) != 3) result = 1;
    return result;
}
