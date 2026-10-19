// C interface of the multilink library (src/ffi.rs), linked statically into Multilink.app.
// Strings returned through `char **` or as `char *` are freed with multilink_free_string.
#ifndef MULTILINK_H
#define MULTILINK_H

typedef struct Running multilink_running;

// Check a config file: NULL if valid, else a one-line reason.
char *multilink_check_config(const char *path);

// Start balancing on a thread of this process (needs root). config_toml is the config's text;
// status (may be NULL) overrides its status address. NULL on failure, reason in *error.
multilink_running *multilink_start(const char *config_toml, const char *status, char **error);

// Apply a new config. 0: all applied. 1: policy, weights and pins applied; *message lists the
// settings that need a restart. -1: invalid config, nothing changed; *message says why.
int multilink_reload(const multilink_running *r, const char *config_toml, char **message);

// Ask the balancer to stop; returns at once.
void multilink_stop(const multilink_running *r);

// Block until stopped and the routes are removed; returns the exit code (0 ok, 1 failure,
// 101 panic). May be called more than once, from any thread.
int multilink_wait(const multilink_running *r);

// Free after multilink_wait has returned.
void multilink_release(multilink_running *r);

void multilink_free_string(char *s);

#endif
