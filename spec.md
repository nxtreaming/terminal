# AI Browser agent

## Harness from LLM to End User

I want to create a new harness SPECFICIALLY for browser use (using the browser)

The main idea is this: give the LLM complete freedom to do whatever it wants. Instead of creating super super robust harness with extremely long
and complicated tools, just let it rip and create whatever the model wants. (pls read this https://browser-use.com/posts/bitter-lesson-agent-
harnesses). MAKE SURE TO REALLY UNDERSTAND THIS REPO, BECAUSE everything you built will be just extending this browser harness concepts to a full blown llm framework -> essentially it's a harness that controls the browser via CDP (for example the concept of a deamon is part of the framework, everything else is completel freedom).

The idea: browser use can be achieved by LLM controlling Chrome via CDP websocket. Everything should be built with that in mind. LLMs know CDP at this point. We should just give it the perfect helper methods to achieve whatever it wants, even if it has to be done via random ways to interact with computer (for example sometimes it just writes some random applescript and executes it in order to get the cdp connection unstuck.) -> so main idea: make sure that we enable the full domain of interaction, so that no tool calls are bound to just one way of doing stuff -> because that can break. Everything has to be extremely generalizable.

WE NEED TO GIVE THE LLM COMPLETE FREEDOM TO DO WHATEVER IT WANTS THE WAY IT WANTS IT -> the main idea of the project is to extent browser harness idea of self improvements and simplistic harness to a full blown harness for the LLM -> it should NOT be simple to the point of not having robust tool implementationts, but it should be simple in terms of concepts that it allows agents to self improve etc. Does this make sense?

/Users/greg/Documents/browser-use/hackathons/harnesless -> this is the repo behind the browser harness - it works EXTREMELY well with claude code/
codex/opencode (known as coding harnesses). BUT there are a few problems:
- the interface to the LLM is very bad -> there is a lot of shit on top that we don't need (especially talking about system prompts regarding git, proper hardcore coding etc) -> the llm should have some basic capacity to edit code, interact with bash, edit and read files, and all other things that make coding agents amazing, but probably some features are not needed to interact with the CDP websocket.
- every time we need to read a screenshot it requires an extra LLM call (since you can't chain screenshots), so it can get very expensive and slow
- with coding harnesses there is a lot of extra garbage on top that we never need.
- it's very very slow, because every so many sequantial tool calls to achieve tasks gets extremely fucking slow.
- it's very expensive (if using the API, if we authenticate with codex the plans are obviously free haha)

I want to build a new harness SPECIFICALLY for brwoser use. I want to use the same notion as I used in browser harness - so everything should be
build around the CDP websocket. It should be as SIMPLE AS POSSIBLE, so that the LLM can always edit the files and adds whatever it wants to the
harness and any point (so kind of a python REPl, but make sure that any component can be swapped at any point).

We have to build everything from scratch, from the LLM interation, to agent loop, to the terminal UI. that in the end will display the stuff for
the agent. -> remember: the files that the LLM will read in order to undersatnd the structure of the code have to OPTIMIZED LIKE HELL, so that
adding shit to those files is extremely simple. For other files that handle robust parts of the code this requirement can easily be dropped (like
handling terminal UI (i guess it's super hard to write textual efficinetly)

## Agent

Please read how opencode and pi mono work
 /Users/greg/Downloads/tmp/opencode
 /Users/greg/Downloads/tmp/pi-mono

Make sure that the agent is heavily inspired by how we build the structure in /Users/greg/Documents/browser-use/core/cloud/packages/bu (same structure) BUT with the difference that:
- you should remove the BU -> only have the agent
  - i don't like how subagents are handled -> they should be normal sessions, like the main session that runs in the background (can you please learn how opencode does their subagents -> they do them extremely efficiently)

I really like the code structure that i built myself with packags/bu so try to keep it similar that.

## Other parts of the spec

1. we need to build codex auth (so you can interact with openai model directly using the codex subscription (just check how hermes agent/openclaw does this))
2. let's build an extremely simple terminal ui so that you can easily understand what's going on
3. the agent primitive that i already build in packags/bu is already good with its hooks etc but if you feel the need to change something feel free.
4. compaction, subagents, auto tool call save to a file (if output too big), [[what else]] are the neccesary features of this!!
5. what else does a browser agent need if implemented this way from scratch?

# Complete freedom

Based on what I want from the implementation plan - what else would a web agent actually need in order to work extremely well? Which parts of the agent have to be robust and well implementd, which parts can be loaded in dynamically?
Can we reuse anything or should we just implement everything from scratch?

# End product

I want to interact with chrome via a terminal UI. The UI should have sessions that I can interact with, start sessions, etc. Make sure to build this in the end when you are sure that everything works as planned.

## Getting there

When you are implementing the make sure to test the app thoroughly and make sure that all the browser interaction works!! You can test this super easily using the dataset real v8 (easier tasks) or real_v14 (hard tasks) -> when you have a harness that thing has a lot of exmaple tasks.
