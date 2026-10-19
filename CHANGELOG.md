# Changelog

## 0.1.5

- **Release:** `scripts/release.sh` ships the `/nix/store` files that a binary built in the Nix shell
  needs, since the image has none of them, so the release binary starts in the Debian image. It stops
  with a message when `nix-store` is missing, and tags `latest` only after `codeyard --version` runs in
  the new image.
- **Git:** a failover promotion no longer loses a push that was still being written. A repository
  with a push in progress serves no replica copy, and pushes to one repository share the ref limit.
  Access is checked again after a wait for a session slot, and shutdown waits for the Git sessions
  other nodes have open.
- **CI:** a claim whose reply was lost returns the run the worker already holds, a finished run's
  result is retried until it is recorded, and pull request runs can be run again.
- **Stacks:** a rebase after the layer below was amended replays only the layer's own commits, so
  the old commit of the layer below no longer conflicts.
- **Drafts:** a draft survives a refused submit, such as an expired form. A draft typed before the
  page changed is shown with a notice to review it before saving, and "Use the current text" drops it.
- **Database:** the schema is one baseline again, without the wiki tables. A database made by 0.1.2 to 0.1.4
  does not open with this build; start from a fresh one.
- **Fixed:** webhook edits check the person's rights in the same transaction as the write.
- **Removed:** the repository wiki and the insights page.

## 0.1.4

- **CI:** runs of the same workflow run side by side, with a taken name made unique, so two pushes
  no longer fail on a taken container name. Each job's container stops in one second instead of
  ten, and the runner image is not pulled again on every run. An output line over 16 KiB is cut
  short instead of failing the run, cancelled runs are not reported as errors, only failed runs
  send a notification, and an empty required input is refused. The pull request merge box says
  when checks failed or are still running, and its runs show the head branch. The run page keeps a multi-line `run:` step in one row with a rounded time, and the job
  result line stays with the last step.
- **Git:** a push no longer reports a failure when another push to the same repository ran at the
  same time. Clones and pushes above the session limit wait up to 30 seconds for a slot instead of
  failing at once, and the limit follows the CPU count (`GIT_SESSION_LIMIT`).
- **Fixed:** two-factor sign-in on a private node, and a pending two-factor sign-in ends when the
  password changes; the setup secret is rate limited; webhooks ignore proxy settings; a release
  asset's file can only be deleted with its release; CODEOWNERS comments no longer name owners;
  release uploads over 4 MiB; the PostgreSQL session list with an IP address; deleted comments take
  their mention notices with them; task boxes in pull request comments; pull request comments
  send webhooks; a `**/` path filter matches the top level; a CI log line cut inside a character;
  a changed default branch reaches replicas; wiki Preview without JavaScript no longer saves;
  tag downloads use the tag; unsent drafts stay with the account that typed them; PostgreSQL
  lock-order deadlocks and the assignee limit; milestone due dates in a database copy.

## 0.1.3

- **Releases:** a Releases block on the Code page and a Releases | Tags switch. Create, edit and delete releases on a
  tag, with notes, a pre-release flag and uploaded files. Branch and tag archives download as
  `.zip` or `.tar.gz`.
- **Webhooks:** repository and organization webhooks with a signing secret, a delivery log,
  redelivery and a test ping, for push, issue, pull request and comment events.
- **Two-factor sign-in:** authenticator-app codes and one-time backup codes, set up under
  account settings. An administrator can reset a locked-out account.
- **Deploy keys:** read-only or write SSH keys that give one repository access, for build
  servers. A write key never bypasses branch protection.
- **Templates:** Markdown issue templates, YAML issue forms (input, text area, dropdown and
  checkboxes, with required fields) and a pull request template. Task lists (`- [ ]`) can be
  ticked in place.
- **CODEOWNERS:** owners of the changed files are requested as reviewers on a pull request, and
  again when the base branch changes.
- **Wiki and insights:** a per-repository wiki and a commit-activity page, reachable by URL
  and not shown in the repository tabs.
- **Accounts:** organization invitations, an admin users page, and email and session settings.
- **Database:** additive migrations 2 to 8 on top of the 0.1.2 baseline.

## 0.1.2

- **New interface:** a header with the page context (Dashboard, or owner / repo with the
  repository tabs inside it), a "+" menu, Issues, Pull requests, Repositories and Inbox buttons,
  and a user menu. Actions say what they do with icons and tooltips, in light and dark themes.
- **Files:** the file view has a Files panel, an icon toolbar, a commit bar with History, a
  branch switcher, permalinks and an outline menu. Folder pages share the layout, and history
  can be limited to a path.
- **Issues and pull requests:** GitHub-style lists with search, filter menus and sorting; a
  labels page with colours; label, milestone, assignee and reviewer pickers; a pull request
  Files tab; and a merge box that lists what blocks the merge.
- **Dashboard and accounts:** work cards for your issues and pull requests, a profile without
  the About tab, grouped settings, and an admin overview with statistics over 7, 30, 90 or 180
  days.
- **Search:** every result type covers the repositories you can read, private and organization
  ones included, pages through its results and shows totals on its tab, and sorts across all of
  its matches. Code and commit search walks the most recently updated repositories first. A
  search that runs out of time or repository size says how far it got, names the repositories it
  could not read, and `repo:`, `user:` or `org:` narrow it.
- **Fixed:** a Git push or a web save no longer fails when the branch activity entry cannot be
  recorded, for example with a very long branch name or a busy database.
- **Database:** the schema history is one baseline per database. A database created by an
  earlier build is refused at start-up and is not upgraded in place.
- **Source layout:** tests are grouped by area, and the API and web page folders now follow the
  same URL names.

## 0.1.1 - first public release

Codeyard is a self-hosted Git forge in one Rust application. Pages render on the server
and hydrate in the browser.

- **Git:** repositories over SSH (built-in server or host OpenSSH) and HTTPS with personal
  access tokens. Forks, pull requests from forks, protected branches, stacked pull requests,
  and merges by merge commit, squash or rebase (squash and rebase need Git 2.40 or later).
- **Collaboration:** issues, pull requests with reviews and inline comments, labels,
  milestones and assignees, organizations and teams (deleting an organization deletes its repositories), notifications, a web file editor with
  uploads, file deletion, ownership transfer, and blame and history views. Profiles show recent
  activity, and repository search also matches the owner name.
- **CI:** workflows in the style of GitHub Actions, run through `act` in Podman containers or
  on the host, with secrets, per-workflow statuses and live logs. Manual runs take the inputs a
  workflow declares under `on.workflow_dispatch.inputs`.
- **Protection:** a force push is refused for everyone, administrators and the owner included,
  unless the matching branch rule lists the user in `force_push_allowances`. Branches without
  a rule are unrestricted.
- **Crawlers:** AI crawlers are blocked by default through `robots.txt`, `ai.txt` and an
  `X-Robots-Tag: noai, noimageai` header. Administrators can allow them in site settings.
- **Operations:** SQLite by default and PostgreSQL optionally (`codeyard db copy` moves data
  between them); uploads on disk or in an S3-compatible bucket; a server image and a Compose
  file.
- **Clusters:** web, SSH, storage and CI-worker roles on separate machines, with repository
  replicas.

### Known limits

- No independent security audit. Run workflows only from people you trust; anyone who can use
  the Podman socket can run code as the host user.
- The server sends no email, so there is no password reset by email.
- Automatic maintenance does not reclaim disk space.
