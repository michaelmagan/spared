# Isolation

This page tells you what spared keeps apart today, what it does not yet, and where it is going. Read it before you decide what to host.

## What you get today

Each app runs in its own sandbox. Its SQLite database and its files live in its own data directory, and the app sees only that directory. It cannot read another app's data, the server's files or the machine's environment. SQLite runs inside the same sandbox as your code, so a bug in it stays in that app.

An app has no network of its own. It cannot open sockets or look up names, and it can make HTTP requests only to the hosts its manifest lists. Each request is held to limits on memory, compute time, wall-clock time and outbound calls. An escape suite of 50 cases, 43 of them attacks, was denied in full.

## What is not guaranteed yet

- **Shared machine resources.** Limits apply to one request or to one app's stored data. One app can still use a lot of CPU, disk bandwidth or network over time and slow its neighbours.
- **Builds run on your machine.** Compiling an app runs its build scripts with your privileges, outside the sandbox. Build code you did not write in a container or a VM.
- **No outside audit.** Nobody outside the project has audited the sandbox, and a bug in the engine under it would defeat it. Do not host code that would cause serious harm if the sandbox failed.
- **The admin token controls every app.** Per-app tokens limit a deploy pipeline to one app and the scopes you pick ([operating](operating.md#give-each-app-its-own-token)), but whoever holds the admin token can still change every app, and an app token still uploads code that you then run.

## Where it is going

We want "can I run code I did not write?" to be a plain yes. The order of work is stronger limits first, with a share of CPU, disk and network for each app. Per-app tokens are built: a token can change one app and nothing else. After that comes running untrusted code, backed by an outside audit.

Until then, spared fits small apps that you or your coding agent wrote. The [FAQ](faq.md#can-it-run-untrusted-code-safely) lists what has been tested, and [how it works](how-it-works.md#the-sandbox-in-one-paragraph) describes the sandbox in more detail.
