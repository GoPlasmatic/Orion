#!/usr/bin/env bash
# Suite: Cron channels
#
# A schedule through the CLI against a real server: deploy a cron channel,
# watch occurrences appear on their own, trigger one by hand with
# `cron trigger`, and read the ledger back. This is the wiring the
# in-process tests cannot cover — the CLI's own commands, over HTTP, against
# a server that started its scheduler the way production does.
#
# The schedule fires every second so nothing here waits on a guessed duration.

begin_suite "Cron channels"

_rollup_workflow() {
    echo '{"name":"Scheduled rollup","condition":true,"tasks":[
      {"id":"parse","name":"parse","function":{"name":"parse_json","input":{"source":"payload","target":"input"}}},
      {"id":"summarise","name":"summarise","function":{"name":"map","input":{"mappings":[
        {"path":"data.window","logic":{"var":"data.input.window"}},
        {"path":"data.covers","logic":{"var":"metadata.trigger.scheduled_for"}},
        {"path":"data.trigger_type","logic":{"var":"metadata.trigger.type"}}
      ]}}}
    ]}'
}

# _cron_set <name> <channel json> — a set of the rollup workflow and one cron
# channel bound to it; prints the set's directory.
_cron_set() {
    local dir
    dir=$(new_set "$1")
    set_workflow "$dir" rollup "$(_rollup_workflow)"
    echo "$2" > "$dir/channel.json"
    echo "$dir"
}

# A cron channel firing every second.
EVERY_SECOND_CHANNEL='{
  "channel_id": "e2e-cron", "name": "e2e-cron",
  "channel_type": "async", "protocol": "cron", "workflow_id": "rollup",
  "transport_config": {
    "schedule": "* * * * * *",
    "payload": { "window": "previous_day" },
    "concurrency": { "policy": "forbid" }
  }
}'

# Poll `cron list` until at least $1 occurrences of e2e-cron have reached $2,
# or give up.
_wait_for_occurrences() {
    local want="$1" status="$2" deadline=$((SECONDS + 20))
    while [ $SECONDS -lt $deadline ]; do
        cli cron list --channel-id e2e-cron --status "$status" --limit 50 >/dev/null 2>&1
        local count
        count=$(echo "$CLI_OUTPUT" | jq -r '.data | length' 2>/dev/null || echo 0)
        [ "${count:-0}" -ge "$want" ] && return 0
        sleep 0.5
    done
    return 1
}

test_cron_channel_fires_on_its_schedule() {
    deploy_ok "$(_cron_set every-second "$EVERY_SECOND_CHANNEL")"

    # Nobody triggers anything: the schedule does.
    local waited=0
    _wait_for_occurrences 1 completed || waited=1
    assert_eq "$waited" "0" "no occurrence completed within 20s"

    cli cron list --channel-id e2e-cron --status completed --limit 5
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" ".data[0].channel_name" "e2e-cron"
    assert_json_eq "$CLI_OUTPUT" ".data[0].trigger" "cron"
    assert_json_eq "$CLI_OUTPUT" ".data[0].attempt" "1"

    # The status view knows what is scheduled and when it next fires.
    cli cron status
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '.data[] | select(.channel_id == "e2e-cron") | .schedule' "* * * * * *"

    # The occurrence carries the trace its run wrote, and the run saw its own
    # trigger metadata.
    local occurrence_id trace_id
    cli cron list --channel-id e2e-cron --status completed --limit 1
    occurrence_id=$(echo "$CLI_OUTPUT" | jq -r '.data[0].id')
    cli cron get "$occurrence_id"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" ".data.status" "completed"
    trace_id=$(echo "$CLI_OUTPUT" | jq -r '.data.trace_id')
    assert_ne "$trace_id" "null" "the occurrence must name its trace"

    # `traces get` unwraps the admin envelope, so the trace object is the root.
    cli traces get "$trace_id"
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" ".mode" "cron"
    # And the run saw its own trigger metadata and its authored payload.
    assert_json_eq "$CLI_OUTPUT" ".message.trigger_type" "cron"
    assert_json_eq "$CLI_OUTPUT" ".message.window" "previous_day"

    # A deployment that leaves the channel out stops its schedule: the status
    # view no longer lists it.
    undeploy_all
    cli cron status
    assert_exit_code 0 "$CLI_EXIT"
    assert_json_eq "$CLI_OUTPUT" '[.data[] | select(.channel_id == "e2e-cron")] | length' "0"
}

test_cron_channel_can_be_triggered_by_hand() {
    # A schedule that will not fire during this test, so the only occurrence is
    # the one we ask for.
    deploy_ok "$(_cron_set manual '{
      "channel_id": "e2e-cron-manual", "name": "e2e-cron-manual",
      "channel_type": "async", "protocol": "cron", "workflow_id": "rollup",
      "transport_config": { "schedule": "0 0 4 1 1 *", "payload": { "window": "manual" } }
    }')"

    cli cron trigger e2e-cron-manual
    assert_exit_code 0 "$CLI_EXIT" "trigger: $CLI_STDERR"
    assert_json_eq "$CLI_OUTPUT" ".data.trigger" "manual"
    local occurrence_id
    occurrence_id=$(echo "$CLI_OUTPUT" | jq -r '.data.id')

    local deadline=$((SECONDS + 20)) status=""
    while [ $SECONDS -lt $deadline ]; do
        cli cron get "$occurrence_id" >/dev/null 2>&1
        status=$(echo "$CLI_OUTPUT" | jq -r '.data.status')
        [ "$status" = "completed" ] && break
        sleep 0.5
    done
    assert_eq "$status" "completed"

    # Only a channel the head deploys can be triggered.
    cli cron trigger no-such-channel
    assert_ne "$CLI_EXIT" 0 "triggering a channel the head does not deploy must fail"
}

test_cron_channel_refuses_caller_shaped_config() {
    # A cron channel has no caller, so `auth` would be a setting Orion stores
    # and never applies. Refused at compile instead.
    deploy_set "$(_cron_set with-auth '{
      "channel_id": "e2e-cron-bad", "name": "e2e-cron-bad",
      "channel_type": "async", "protocol": "cron", "workflow_id": "rollup",
      "transport_config": { "schedule": "0 15 2 * * *" },
      "config": { "auth": { "mode": "api_key", "keys": ["k"] } }
    }')"
    assert_exit_code 1 "$CLI_EXIT" "a cron channel must not accept auth"
    assert_contains "$CLI_STDERR" "must not set auth"

    # And a five-field expression, which means something else entirely.
    deploy_set "$(_cron_set five-fields '{
      "channel_id": "e2e-cron-bad", "name": "e2e-cron-bad",
      "channel_type": "async", "protocol": "cron", "workflow_id": "rollup",
      "transport_config": { "schedule": "15 2 * * *" }
    }')"
    assert_exit_code 1 "$CLI_EXIT" "a five-field expression must be refused"
    assert_contains "$CLI_STDERR" "exactly 6 fields"
}

run_test "cron: a schedule fires, runs and records an occurrence" test_cron_channel_fires_on_its_schedule
run_test "cron: cron trigger runs a deployed channel now"         test_cron_channel_can_be_triggered_by_hand
run_test "cron: caller-shaped config and bad expressions are refused" test_cron_channel_refuses_caller_shaped_config

end_suite
