## DNEW-F-win-afunix Local sockets on Windows use its own `AF_UNIX` sockets, with an owner-only access list in place of mode 0600 — Decided by the build loop (by judgement, 2026-11-02); refines D164

D164 left `eggdb serve --socket-dir` and `--mysql-socket` unavailable on Windows. Windows 10
version 1803 and Windows Server 2019 added `AF_UNIX` stream sockets that live as files in the
file system, which libpq 14 and later and several drivers already use.

- **Platform layer.** `eggdb_platform::local_socket` gains a Windows implementation on our own
  Winsock declarations (`WSASocketW(AF_UNIX, SOCK_STREAM)`, `bind` with a `sockaddr_un` of a
  `u16` family and a 108-byte path, `listen`, `accept`, `connect`, `recv`, `send`, `setsockopt`
  for timeouts, `ioctlsocket(FIONBIO)` and `shutdown`). `LocalListener` and `LocalStream` keep
  the Unix API the servers use (accept, non-blocking mode, timeouts, shutdown, `try_clone`,
  `Read` and `Write` on the stream and on `&stream`), so the call sites stay free of `cfg`. The
  path is passed as its UTF-8 bytes and must fit in 107 bytes with its terminator.
- **No peer address or credentials.** Windows reports neither for these sockets, so `accept`
  returns `()` beside the stream there. Nothing in EggDB reads either on Unix today.
- **Stale and live sockets.** A socket file is recognised by its `IO_REPARSE_TAG_AF_UNIX`
  reparse tag, read through a handle opened on the reparse point itself. A connect test then
  tells a live server (refused with `InUse`) from a stale file (deleted and replaced), as on
  Unix; any other file is refused as not a socket.
- **Owner only.** After `bind`, the socket file's DACL is replaced by a protected one that grants
  `FILE_ALL_ACCESS` to the user in the process token and nobody else, the stand-in for mode
  0600 (connecting needs write access to the file). If the access list cannot be set, the socket
  is closed and its file deleted, so no socket is ever left open to others.
- **Older Windows.** `WSAEAFNOSUPPORT` from the socket call is reported as
  `LocalBindError::Unsupported`, and `eggdb serve` answers with exit 2 saying the option needs
  Windows 10 version 1803 or later.
- **MySQL clients.** The mysql client on Windows reads `--socket` as a named pipe name, so
  `--mysql-socket` there serves drivers that open `AF_UNIX` paths. Named pipes are a separate
  transport, not built here.
