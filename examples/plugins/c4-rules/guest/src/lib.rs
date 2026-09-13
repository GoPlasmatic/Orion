//! `c4.rules`: the rules of Connect Four as three pure functions, so a
//! workflow can referee a game without spelling a board in JSONLogic.
//!
//! The state is one JSON object:
//!
//! ```json
//! { "board": [42 ints], "to_move": 1, "moves": 0, "over": false, "winner": 0, "illegal": false }
//! ```
//!
//! `board` is row-major, six rows of seven, **row 0 at the top**: cell
//! `r * 7 + c` is row `r`, column `c`, and a disc dropped in a column lands
//! in the empty cell with the largest row index. `0` is empty, `1` is
//! player one's disc, `2` is player two's. `to_move` is the player who moves
//! next, `moves` is the number of discs on the board (informational — it is
//! recomputed from the board, never read), and once `over` is set `winner`
//! is `1`, `2`, or `0` for a draw.
//!
//! - `c4.rules.start`: the initial state — an empty board, player one to
//!   move. Takes no input.
//! - `c4.rules.view`: `state` → `{cells, legal}` from the mover's
//!   perspective: `cells` is the board with the mover's discs as `1` and the
//!   opponent's as `2`, `legal` is seven `0`/`1` flags, one per column. This
//!   is the contract an entrant model is written against.
//! - `c4.rules.apply`: `state` + `column` → the next state. `column` is an
//!   integer, or a one-element list holding one (what `argmax` over a
//!   `[1, 7]` policy yields). Four in a row in any direction wins; a 42nd
//!   disc with no winner is a draw; a column that is full or out of range is
//!   a **forfeit** — `illegal` and `over` are set, the board is unchanged,
//!   and the *other* player is the winner.
//!
//! Every refusal is `caller-input`: the state or the column were not the
//! shape above, and the same input cannot succeed on a retry.

// Off the wasm target nothing but the tests reaches the rules: the export
// below is what references them, and it exists only in a component.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use orion_plugin_sdk::{Plugin, PluginError, Value, json, serde_json};

const ROWS: usize = 6;
const COLS: usize = 7;
const CELLS: usize = ROWS * COLS;

struct Rules;

#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    board: [u8; CELLS],
    to_move: u8,
    over: bool,
    winner: u8,
    illegal: bool,
}

fn bad_state(message: impl Into<String>) -> PluginError {
    PluginError::caller_input("BAD_STATE", message)
}

fn bad_column(message: impl Into<String>) -> PluginError {
    PluginError::caller_input("BAD_COLUMN", message)
}

fn other(player: u8) -> u8 {
    3 - player
}

impl State {
    fn start() -> Self {
        Self {
            board: [0; CELLS],
            to_move: 1,
            over: false,
            winner: 0,
            illegal: false,
        }
    }

    /// Discs on the board.
    fn moves(&self) -> usize {
        self.board.iter().filter(|cell| **cell != 0).count()
    }

    fn to_value(&self) -> Value {
        json!({
            "board": self.board.iter().map(|cell| u64::from(*cell)).collect::<Vec<_>>(),
            "to_move": self.to_move,
            "moves": self.moves(),
            "over": self.over,
            "winner": self.winner,
            "illegal": self.illegal,
        })
    }

    /// The `state` field of an input. `board` and `to_move` are required;
    /// `over`, `winner` and `illegal` default to a game in progress, and
    /// `moves` is ignored because the board already says it.
    fn parse(input: &Value) -> Result<Self, PluginError> {
        let state = input
            .get("state")
            .and_then(Value::as_object)
            .ok_or_else(|| bad_state("'state' must be an object"))?;
        let cells = state
            .get("board")
            .and_then(Value::as_array)
            .ok_or_else(|| bad_state("'state.board' must be an array of 42 cells"))?;
        if cells.len() != CELLS {
            return Err(bad_state(format!(
                "'state.board' has {} cells, a board has {CELLS}",
                cells.len()
            )));
        }
        let mut board = [0u8; CELLS];
        for (i, cell) in cells.iter().enumerate() {
            board[i] = match cell.as_u64() {
                Some(disc @ 0..=2) => disc as u8,
                _ => return Err(bad_state(format!("'state.board[{i}]' must be 0, 1 or 2"))),
            };
        }
        let to_move = match state.get("to_move").and_then(Value::as_u64) {
            Some(player @ 1..=2) => player as u8,
            _ => return Err(bad_state("'state.to_move' must be 1 or 2")),
        };
        let winner = match state.get("winner") {
            None | Some(Value::Null) => 0,
            Some(value) => match value.as_u64() {
                Some(player @ 0..=2) => player as u8,
                _ => return Err(bad_state("'state.winner' must be 0, 1 or 2")),
            },
        };
        Ok(Self {
            board,
            to_move,
            over: flag(state, "over")?,
            winner,
            illegal: flag(state, "illegal")?,
        })
    }

    /// The cell a disc dropped in `column` lands in, or `None` when the
    /// column is full or does not exist.
    fn landing(&self, column: i64) -> Option<usize> {
        let column = usize::try_from(column).ok().filter(|c| *c < COLS)?;
        (0..ROWS)
            .rev()
            .map(|row| row * COLS + column)
            .find(|cell| self.board[*cell] == 0)
    }

    /// Whether the disc at `cell` completes four in a row in any direction.
    fn wins_at(&self, cell: usize) -> bool {
        let (row, col) = ((cell / COLS) as i64, (cell % COLS) as i64);
        let player = self.board[cell];
        let run = |dr: i64, dc: i64| {
            let (mut r, mut c, mut n) = (row + dr, col + dc, 0);
            while (0..ROWS as i64).contains(&r)
                && (0..COLS as i64).contains(&c)
                && self.board[r as usize * COLS + c as usize] == player
            {
                n += 1;
                r += dr;
                c += dc;
            }
            n
        };
        [(0, 1), (1, 0), (1, 1), (1, -1)]
            .into_iter()
            .any(|(dr, dc)| 1 + run(dr, dc) + run(-dr, -dc) >= 4)
    }
}

fn flag(state: &serde_json::Map<String, Value>, key: &str) -> Result<bool, PluginError> {
    match state.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(bad_state(format!("'state.{key}' must be a boolean"))),
    }
}

/// The `column` field: an integer, or a one-element list holding one.
fn column(input: &Value) -> Result<i64, PluginError> {
    let raw = input
        .get("column")
        .ok_or_else(|| bad_column("'column' is required"))?;
    let value = match raw {
        Value::Array(items) if items.len() == 1 => &items[0],
        Value::Array(items) => {
            return Err(bad_column(format!(
                "'column' as a list must hold exactly one number, got {}",
                items.len()
            )));
        }
        other => other,
    };
    let number = value.as_f64().ok_or_else(|| {
        bad_column("'column' must be a number, or a one-element list holding one")
    })?;
    if number.fract() != 0.0 || !number.is_finite() {
        return Err(bad_column(format!(
            "'column' must be an integer, got {number}"
        )));
    }
    Ok(number as i64)
}

fn start() -> Value {
    State::start().to_value()
}

fn view(input: &Value) -> Result<Value, PluginError> {
    let state = State::parse(input)?;
    let mover = state.to_move;
    let cells: Vec<u8> = state
        .board
        .iter()
        .map(|&disc| match disc {
            0 => 0,
            disc if disc == mover => 1,
            _ => 2,
        })
        .collect();
    let legal: Vec<u8> = (0..COLS)
        .map(|col| u8::from(!state.over && state.board[col] == 0))
        .collect();
    Ok(json!({ "cells": cells, "legal": legal }))
}

fn apply(input: &Value) -> Result<Value, PluginError> {
    let mut state = State::parse(input)?;
    let column = column(input)?;
    if state.over {
        return Err(PluginError::caller_input(
            "GAME_OVER",
            "the game is over; start a new one with c4.rules.start",
        ));
    }
    let mover = state.to_move;
    match state.landing(column) {
        None => {
            state.illegal = true;
            state.over = true;
            state.winner = other(mover);
        }
        Some(cell) => {
            state.board[cell] = mover;
            if state.wins_at(cell) {
                state.over = true;
                state.winner = mover;
            } else if state.moves() == CELLS {
                state.over = true;
                state.winner = 0;
            }
            state.to_move = other(mover);
        }
    }
    Ok(state.to_value())
}

impl Plugin for Rules {
    fn invoke(function: &str, input: Value) -> Result<Value, PluginError> {
        match function {
            "c4.rules.start" => Ok(start()),
            "c4.rules.view" => view(&input),
            "c4.rules.apply" => apply(&input),
            other => Err(PluginError::caller_input(
                "UNKNOWN_FUNCTION",
                format!("this component exports no '{other}'"),
            )),
        }
    }
}

// The component export. Gated on the wasm target so `cargo test` can link
// the rules into a host test binary: the export shim references symbols only
// a component has.
#[cfg(target_arch = "wasm32")]
orion_plugin_sdk::export_plugin!(Rules);

#[cfg(test)]
mod tests {
    use super::*;

    fn call(function: &str, input: Value) -> Result<Value, PluginError> {
        Rules::invoke(function, input)
    }

    /// Apply `columns` in order from `state`, returning the last state.
    fn play(mut state: Value, columns: &[i64]) -> Value {
        for column in columns {
            state = call(
                "c4.rules.apply",
                json!({ "state": state, "column": column }),
            )
            .unwrap_or_else(|e| panic!("column {column}: {e}"));
        }
        state
    }

    fn fresh() -> Value {
        call("c4.rules.start", json!({})).expect("start")
    }

    #[test]
    fn start_is_an_empty_board_with_player_one_to_move() {
        let state = fresh();
        assert_eq!(state["board"].as_array().expect("board").len(), 42);
        assert!(state["board"].as_array().unwrap().iter().all(|c| c == 0));
        assert_eq!(state["to_move"], 1);
        assert_eq!(state["moves"], 0);
        assert_eq!(state["over"], false);
        assert_eq!(state["winner"], 0);
        assert_eq!(state["illegal"], false);
    }

    #[test]
    fn a_disc_falls_to_the_bottom_of_its_column_and_the_turn_passes() {
        let state = play(fresh(), &[3]);
        assert_eq!(state["board"][5 * 7 + 3], 1, "bottom row of column 3");
        assert_eq!(state["to_move"], 2);
        assert_eq!(state["moves"], 1);
        let state = play(state, &[3]);
        assert_eq!(state["board"][4 * 7 + 3], 2, "stacked on top");
        assert_eq!(state["to_move"], 1);
        assert_eq!(state["moves"], 2);
        assert_eq!(state["over"], false);
    }

    #[test]
    fn four_in_a_row_horizontally_wins() {
        let state = play(fresh(), &[0, 0, 1, 1, 2, 2, 3]);
        assert_eq!(state["over"], true);
        assert_eq!(state["winner"], 1);
        assert_eq!(state["illegal"], false);
    }

    #[test]
    fn four_in_a_row_vertically_wins() {
        let state = play(fresh(), &[0, 1, 0, 1, 0, 1, 0]);
        assert_eq!(state["over"], true);
        assert_eq!(state["winner"], 1);
    }

    #[test]
    fn four_in_a_row_on_either_diagonal_wins() {
        let rising = [0, 1, 1, 2, 2, 3, 2, 3, 3, 0, 3];
        let state = play(fresh(), &rising);
        assert_eq!(state["over"], true, "{state}");
        assert_eq!(state["winner"], 1);

        let falling: Vec<i64> = rising.iter().map(|c| 6 - c).collect();
        let state = play(fresh(), &falling);
        assert_eq!(state["over"], true, "{state}");
        assert_eq!(state["winner"], 1);
    }

    #[test]
    fn player_two_can_win_too() {
        // One wastes tempo in column 6 while two builds a column.
        let state = play(fresh(), &[6, 0, 6, 0, 6, 0, 5, 0]);
        assert_eq!(state["over"], true);
        assert_eq!(state["winner"], 2);
    }

    #[test]
    fn the_forty_second_disc_with_no_winner_is_a_draw() {
        // A full board with no four in a row, minus player one's disc at the
        // top of column 6 — the last cell to fill.
        let full = [
            2, 1, 1, 2, 2, 2, 1, 1, 2, 2, 2, 1, 2, 2, 2, 1, 1, 2, 1, 2, 1, 1, 2, 2, 1, 2, 1, 1, 1,
            1, 2, 1, 2, 1, 2, 2, 1, 2, 1, 2, 1, 1,
        ];
        let mut board = full.to_vec();
        board[6] = 0;
        let state = json!({ "board": board, "to_move": 1 });
        let state = play(state, &[6]);
        assert_eq!(state["moves"], 42);
        assert_eq!(state["over"], true);
        assert_eq!(state["winner"], 0);
        assert_eq!(state["illegal"], false);
        assert_eq!(state["board"][6], 1);
    }

    #[test]
    fn a_full_column_is_a_forfeit() {
        let six_in_column_zero = play(fresh(), &[0, 0, 0, 0, 0, 0]);
        assert_eq!(
            six_in_column_zero["over"], false,
            "alternating discs do not win"
        );
        assert_eq!(six_in_column_zero["to_move"], 1);
        let state = play(six_in_column_zero.clone(), &[0]);
        assert_eq!(state["illegal"], true);
        assert_eq!(state["over"], true);
        assert_eq!(state["winner"], 2, "the other player wins");
        assert_eq!(
            state["board"], six_in_column_zero["board"],
            "nothing was placed"
        );
        assert_eq!(state["moves"], 6);
    }

    #[test]
    fn a_column_outside_the_board_is_a_forfeit() {
        for column in [7, -1, 42] {
            let state = play(fresh(), &[column]);
            assert_eq!(state["illegal"], true, "column {column}");
            assert_eq!(state["over"], true);
            assert_eq!(state["winner"], 2);
            assert_eq!(state["moves"], 0);
        }
    }

    #[test]
    fn a_one_element_list_is_accepted_as_the_column() {
        let state =
            call("c4.rules.apply", json!({ "state": fresh(), "column": [3] })).expect("apply");
        assert_eq!(state["board"][5 * 7 + 3], 1);
        let state =
            call("c4.rules.apply", json!({ "state": fresh(), "column": 3.0 })).expect("apply");
        assert_eq!(state["board"][5 * 7 + 3], 1);
    }

    #[test]
    fn the_view_is_from_the_movers_perspective() {
        // Player one in column 3, player two in column 0; player one to move.
        let state = play(fresh(), &[3, 0]);
        let view = call("c4.rules.view", json!({ "state": state.clone() })).expect("view");
        assert_eq!(view["cells"][5 * 7 + 3], 1, "the mover's own disc");
        assert_eq!(view["cells"][5 * 7], 2, "the opponent's disc");
        assert_eq!(view["legal"], json!([1, 1, 1, 1, 1, 1, 1]));

        // The same board seen by player two flips the colours.
        let state = play(state, &[1]);
        let view = call("c4.rules.view", json!({ "state": state })).expect("view");
        assert_eq!(view["cells"][5 * 7 + 3], 2);
        assert_eq!(view["cells"][5 * 7], 1);
        assert_eq!(view["cells"][5 * 7 + 1], 2);
    }

    #[test]
    fn a_full_column_is_not_legal_and_a_finished_game_has_no_legal_moves() {
        let state = play(fresh(), &[0, 0, 0, 0, 0, 0]);
        let view = call("c4.rules.view", json!({ "state": state })).expect("view");
        assert_eq!(view["legal"], json!([0, 1, 1, 1, 1, 1, 1]));

        let won = play(fresh(), &[0, 1, 0, 1, 0, 1, 0]);
        let view = call("c4.rules.view", json!({ "state": won })).expect("view");
        assert_eq!(view["legal"], json!([0, 0, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn refusals_name_what_was_wrong() {
        let err = call("c4.rules.apply", json!({ "column": 1 })).expect_err("no state");
        assert_eq!(err.code, "BAD_STATE");

        let err = call(
            "c4.rules.view",
            json!({ "state": { "board": vec![0; 41], "to_move": 1 } }),
        )
        .expect_err("short board");
        assert_eq!(err.code, "BAD_STATE");
        assert!(err.message.contains("41"), "{err}");

        let err =
            call("c4.rules.apply", json!({ "state": fresh(), "column": "3" })).expect_err("text");
        assert_eq!(err.code, "BAD_COLUMN");

        let err = call(
            "c4.rules.apply",
            json!({ "state": fresh(), "column": [1, 2] }),
        )
        .expect_err("pair");
        assert_eq!(err.code, "BAD_COLUMN");

        let err = call("c4.rules.apply", json!({ "state": fresh(), "column": 2.5 }))
            .expect_err("fraction");
        assert_eq!(err.code, "BAD_COLUMN");

        let won = play(fresh(), &[0, 1, 0, 1, 0, 1, 0]);
        let err = call("c4.rules.apply", json!({ "state": won, "column": 2 })).expect_err("over");
        assert_eq!(err.code, "GAME_OVER");

        let err = call("c4.rules.nope", json!({})).expect_err("unknown");
        assert_eq!(err.code, "UNKNOWN_FUNCTION");
    }
}
