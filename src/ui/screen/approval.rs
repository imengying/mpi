//! Approval rendering and input are owned by the same screen as the composer.
use super::*;
use crate::auth::guard::{ApprovalRequest, Decision};
use crate::ui::auth_panel::{PanelState, panel_body};

impl Screen {
    pub async fn request_approval(&mut self, request: &ApprovalRequest) -> Decision {
        if !self.interactive {
            return Decision::Deny;
        }
        if self.detail.is_some() {
            self.close_detail();
        }
        self.suspend_live();
        let mut body = format!(
            "{}\n目录：{}\n权限：{}",
            panel_body(&request.tool, &request.arguments),
            request.cwd.display(),
            request.scope.label()
        );
        if request.scope == crate::tools::sandbox::Scope::Workspace {
            body.push_str(&format!("\n可写范围：{}", request.workspace.display()));
        }
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        self.approval = Some((PanelState::new(&body), sender));
        let _ = crossterm::execute!(self.out, cursor::Hide);
        self.draw_live();
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(16));
        let decision = loop {
            tokio::select! {
                decision = &mut receiver => break decision.unwrap_or(Decision::Deny),
                _ = tick.tick() => { self.poll_input(); }
            }
        };
        let _ = crossterm::execute!(self.out, cursor::Show);
        self.draw_live();
        decision
    }

    pub(super) fn approval_event(&mut self, event: Event) {
        let decision = match event {
            Event::Key(key) => self
                .approval
                .as_mut()
                .and_then(|(panel, _)| panel.handle_key(key)),
            Event::Resize(width, height) => {
                self.width = usize::from(width.max(1));
                self.height = usize::from(height.max(1));
                None
            }
            _ => None,
        };
        if let Some(decision) = decision
            && let Some((mut panel, sender)) = self.approval.take()
        {
            panel.clear(&mut self.out);
            let _ = sender.send(decision);
        }
    }
}
