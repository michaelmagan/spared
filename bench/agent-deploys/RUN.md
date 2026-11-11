# Fresh-agent test

Protocol: a fresh Sonnet agent, given only README.md and AGENTS.md, attempts 10 fixed prompts. A run counts as working when the deployed app passes the behaviour check in `check.py`; the time is prompt to live URL with a warm build cache.

Setup (once, before the first run):
1. `spared` on PATH, a wasi-sdk under ~/.spared/wasi-sdk, wasm32-wasip2 installed, one scaffold built so ~/.spared/target holds the compiled dependencies (the warm cache).
2. `SPARED_EXTRA_CA_FILE=<ca-bundle.crt> spared serve --data-dir ~/agent-deploys-data` running on the default ports. (The extra CA file is only needed because this network intercepts TLS; prompt 10 fetches an HTTPS site.)
3. For each prompt N: a new empty directory ~/agent-deploys/N containing a copy of README.md and AGENTS.md and nothing else.

Per run: spawn one fresh Sonnet agent with the instruction below, one at a time, nothing else running. Time = the agent's total duration. Then `python3 bench/agent-deploys/check.py N` decides pass or fail from behaviour. A failure is double checked by hand before it is counted.

Agent instruction (fill in <N>, <PROMPT>, <NAME>):

    You are a coding agent. Work only inside ~/agent-deploys/<N>. That directory has README.md and AGENTS.md for a tool called spared. Read those two files and follow them. Do not read other files on this machine outside that directory and the files spared generates for you; the spared source code is not meant for you. The `spared` command is installed and a spared server is already running with default settings.
    Task: <PROMPT> Name the app `<NAME>`.
    When the app is deployed and working, reply with its live URL and one sentence on how you checked it. Then stop.

Limit: we cannot technically stop an agent reading the source tree; the instruction asks it not to. Record any run where the agent's transcript shows it did.
