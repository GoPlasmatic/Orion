//! The one style rule: a certain no-op. Kept small on purpose — a style
//! rule the author disagrees with cannot be silenced, so only the
//! indisputable ships.

use crate::definitions::analysis::Analysis;
use crate::definitions::clippy::{Diagnostic, Group, Level, Rule, Scope};

pub struct TerminalOnLastStep;

impl Rule for TerminalOnLastStep {
    fn id(&self) -> &'static str {
        "style.terminal_on_last_step"
    }
    fn group(&self) -> Group {
        Group::Style
    }
    fn level(&self) -> Level {
        Level::Warn
    }
    fn scope(&self) -> Scope {
        Scope::Workflow
    }
    fn summary(&self) -> &'static str {
        "terminal: true on the last top-level step is a no-op"
    }
    fn explain(&self) -> &'static str {
        "`terminal: true` ends the workflow after the step. On the last top-level step \
         nothing follows, so the flag changes nothing.\n\n\
         Proof: position — there is no later step to skip.\n\n\
         Silent when: the step is not the last of the top-level list (a terminal step at the \
         end of a *group* still ends the workflow early)."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        for wf in &cx.workflows {
            let Some(last) = wf.steps.iter().rev().find(|s| s.parent.is_none()) else {
                continue;
            };
            if last.terminal {
                out.push(
                    Diagnostic::on_workflow(
                        self,
                        cx,
                        wf,
                        Some(&format!("{}.terminal", last.path)),
                        format!(
                            "`{}` is the last step; `terminal: true` on it does nothing",
                            last.id
                        ),
                    )
                    .with_remedy("remove the flag"),
                );
            }
        }
    }
}
