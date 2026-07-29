# Security policy

## Reporting a vulnerability

Please do not open a public issue for a suspected vulnerability.

Use GitHub's **Report a vulnerability** action on the repository's Security
page to start a private security advisory. Include the affected version, a
minimal reproduction or wire transcript, expected impact, and any known
mitigation.

You should receive an acknowledgement within seven days. Fixes are coordinated
privately and released after affected users have a practical upgrade path.

## Supported versions

Before the first public release, security fixes land on `main`. After release,
the newest published version is supported. Older pre-1.0 versions may receive a
fix when a safe backport is practical, but upgrading is the default guidance.

Never include live device credentials, production socket captures, or private
network details in a report.
