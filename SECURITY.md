# Security policy

## Supported versions

abc has no releases yet. Only the `main` branch is supported, and fixes land there.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub. Open the repository's **Security** tab and choose
**Report a vulnerability**, or go straight to
<https://github.com/getabc-dev/abc/security/advisories/new>.

If you cannot use GitHub's reporting, email <snehilvijay.productivity@protonmail.com> instead.

Please do not open a public issue, pull request or discussion for a vulnerability.

Include what you can of:

- the affected component (server, worker, web app, sandbox, GitHub integration, build tooling) and
  the commit you tested;
- what an attacker can do, and from where (the network, a browser, a worker, an agent, a PR);
- steps to reproduce, or a proof of concept;
- any fix or mitigation you suggest.

## What to expect

- We acknowledge your report and keep you updated, in the advisory or by email.
- We confirm the issue, fix it on `main` and add a test that fails without the fix.
- We publish the advisory once the fix has landed, at a time agreed with you, and credit you unless
  you ask us not to.
- abc has no bug bounty.

## Scope

abc runs agents on people's own machines, so we treat these as security issues even when no
release is affected. Each maps to a rule in [AGENTS.md](AGENTS.md) and a section of the
[threat model](docs/threat-model.md).

- abc reading, storing, proxying or logging a user's Claude subscription credentials (I1).
- A production secret reaching a dev environment, an agent, a log or a PR artifact (I2).
- An agent reaching code, files or environments outside its own change without a folder grant, or
  escaping its sandbox (I3).
- An action without a recorded actor, or a request that skips the permission check (I4).
- A monitor or integration holding write access it does not need (I5).
- A PR artifact entry whose author can be forged, or a finding closed by the agent that raised it
  (I6).
- Anything that lets someone without access to your Tailscale network reach the server beyond its
  public webhook path (I4).
- Weaknesses in the build and CI gates: unpinned tools or actions, skipped checks, leaked
  secrets (P3).

Out of scope: vulnerabilities in Claude Code, Codex, Tailscale, GitHub or the operating system
sandboxes themselves. Report those to their vendors. If abc uses one of them unsafely, that is in
scope.
