# Orion documentation usability test

Use this script with developers who have not previously used Orion. It turns
documentation quality into observable tasks rather than an internal opinion.

## Participant profile

Recruit at least five application developers across two experience bands:

- developers comfortable with HTTP APIs and containers but new to workflow
  runtimes;
- platform or backend developers who have used an API gateway, automation
  platform, or durable execution system.

Do not brief participants on Orion terminology before the session.

## Session setup

- Provide a clean machine or disposable development environment with Docker,
  `curl`, and a browser.
- Start from the documentation home page, not a deep link.
- Ask the participant to think aloud. Answer environment questions, but do not
  explain where content lives.
- Record navigation, searches, errors, backtracking, and time to completion.

## Tasks

### 1. Assess fit

“Your team needs an HTTP endpoint that validates an order, reads a customer
record, and returns within a normal request. Decide whether Orion is a suitable
choice and name one situation in which you would choose something else.”

Success: the participant identifies Orion's pipeline model and one explicit
boundary without facilitator help.

### 2. Publish an endpoint

“Run Orion locally and publish the documented first API. Send it a request and
explain which definition contains logic and which exposes the endpoint.”

Success: the participant receives a valid response and correctly identifies
workflow and channel.

### 3. Diagnose a failed workflow

Provide a workflow that omits `parse_json`, then ask: “The endpoint returns an
empty result. Find the cause and the documented correction.”

Success: the participant reaches troubleshooting or workflow-authoring
guidance, identifies the missing parse, and explains why no condition fired.

## Measures

Record for each task:

| Measure | Value |
|---|---|
| Completed without help | yes / no |
| Time to first useful page | |
| Time to completion | |
| Searches and search terms | |
| Backtracks | |
| Incorrect assumptions | |
| Pages used | |
| Participant confidence (1–5) | |

## Observation log

| Participant | Task | Observation | Severity | Proposed documentation change |
|---|---|---|---|---|
| | | | | |

Treat repeated hesitation as evidence even when the task eventually succeeds.
Prioritize issues seen by at least two participants or issues that prevent one
participant from completing a task.

## Exit questions

1. In one sentence, what is Orion?
2. Where would you expect to find an exact field or command?
3. What felt ambiguous or overly detailed?
4. Which next step would you take for your own use case?

## Review cadence

Run this study after major navigation changes and before each minor release.
Store completed observation logs with the release research notes, then turn
accepted findings into tracked documentation issues. Do not mark a finding
resolved until the affected task is rerun with a new participant.

## Measured timings

§11.1.18 asks that a quickstart's stated time be *measured*, not estimated.
Record each measurement here so the next person to change the quickstart can
re-run it rather than guess.

### Quickstart, 2026-09-14

Walked `src/get-started/quickstart.md` verbatim — every command copied from the
page, in order, with no edits — against `ghcr.io/goplasmatic/orion:latest`
(Orion 1.8.0, digest `sha256:8498b81c…`).

| Measurement | Result |
|---|---|
| Host | macOS 15 (Darwin 25.6.0), Docker Engine 29.7.2, curl 8.7.1 |
| Image size | 204 MB |
| Commands, image already local | 2 s wall clock, start to last response |
| Steps that failed | none, after the fix below |
| Output drift | none — every response matched what the page prints |

The page now says the commands run in under 5 seconds once the image is local,
and that the first run also pulls 204 MB. The reader's five minutes is the pull
plus reading and pasting, not the commands.

**One defect the walk found.** The documented readiness probe failed on every
cold start, 3 times out of 3:

```console
$ curl --retry 10 --retry-delay 1 --retry-connrefused -fsS http://localhost:8080/healthz
curl: (56) Recv failure: Connection reset by peer
```

Docker's port proxy accepts the connection before the server is listening and
then resets it. `--retry-connrefused` covers a refused connection, not a reset,
so curl gave up on the first attempt. `--retry-all-errors` (curl 7.71 and
later) covers both, and succeeded 3 times out of 3 from cold. Fixed on the
quickstart and on `get-started/tutorials/orders-api.md`, which carried the same
command.
