# Security policy

ddram installs a process-wide SIGSEGV/SIGBUS handler (Unix) or vectored
exception handler (Windows) and writes application memory to disk. Please
report vulnerabilities privately through GitHub's private vulnerability
reporting ("Report a vulnerability" on the repository's Security tab) rather
than in a public issue. We aim to acknowledge reports within 5 working days.

Notes for users:

* Backing files contain whatever the application stored in the region, in
  plain text. Put them on an encrypted volume if that data is sensitive, and
  delete them after use (temporary backing files are deleted automatically).
* Reading NVMe health logs requires elevated privileges on Linux
  (CAP_SYS_ADMIN); ddram never needs them for paging itself.
