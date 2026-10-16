/* TEST ONLY: unprivileged sshd fixture. No host account/configuration is changed.
 * The test account may have nologin as its shell. Substitute only this same UID's shell
 * inside the disposable sshd process; retain real UID/GID, account and password
 * state. Public-key authentication and forced commands remain real OpenSSH.
 * Compile and use only in the unprivileged OpenSSH test fixture.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pwd.h>
#include <unistd.h>

static struct passwd *fixture_shell(struct passwd *entry) {
    if (entry && entry->pw_uid == getuid() && getuid() != 0)
        entry->pw_shell = "/bin/sh";
    return entry;
}

struct passwd *getpwnam(const char *name) {
    struct passwd *(*original)(const char *) = dlsym(RTLD_NEXT, "getpwnam");
    return original ? fixture_shell(original(name)) : 0;
}

struct passwd *getpwuid(uid_t uid) {
    struct passwd *(*original)(uid_t) = dlsym(RTLD_NEXT, "getpwuid");
    return original ? fixture_shell(original(uid)) : 0;
}
