# Memory

The memory plugin ships as two binaries: `mu-memory`, a small Rust shim that each `mu` session starts, and `mu-memory-daemon`, built by `bun build --compile`. The shim starts one shared daemon per `$MU_HOME` on demand (socket `$MU_HOME/memory/sock`, log `$MU_HOME/logs/memory-daemon.log`) and relays the session to it, so several `mu` sessions share one Bun runtime and embedder. The daemon exits after 10 idle minutes without a session (`MU_MEMORY_IDLE_SECS`).
