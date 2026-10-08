# Security policy

heifer decodes untrusted files, so security issues are taken seriously.

## Supported versions

Security fixes are released for the latest published version (currently 0.1.x).

## Reporting a vulnerability

Please **do not open a public issue** for a vulnerability.
Report it privately through GitHub instead: go to the
[Security tab](https://github.com/HarpeLm/Heifer/security) and click **Report a vulnerability**.

Please include:

- the file that triggers the problem, or a way to produce it;
- the heifer version, and your platform;
- what happens (crash, hang, excessive memory use, wrong output).

You should get a reply within a week. Once a fix is released, the issue will be disclosed in a
GitHub security advisory and credited to you, unless you prefer otherwise.

## Scope

In scope:

- panics, infinite loops, excessive memory or CPU use on a crafted file, despite `Options::max_pixels`;
- any memory-safety issue (heifer forbids `unsafe` code, so this would most likely be in a dependency or the compiler).

Out of scope: decoding mistakes on valid files with no security impact. Please report those as
normal [issues](https://github.com/HarpeLm/Heifer/issues).
