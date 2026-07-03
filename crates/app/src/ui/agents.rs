use crate::storage::{AgentConfigRow, Db};

/// agent command 등록 창 (설계문서 PR-09).
/// 실행(pane attach)은 PR-10에서 — 여기서는 등록/조회/삭제만.
pub struct AgentsUi {
    open: bool,
    name: String,
    command: String,
    /// 줄바꿈으로 구분해 args array로 저장한다 (셸 문자열 파싱 금지)
    args_input: String,
    error: Option<String>,
    cached: Option<Vec<AgentConfigRow>>,
}

impl AgentsUi {
    pub fn new() -> Self {
        Self {
            open: false,
            name: String::new(),
            command: String::new(),
            args_input: String::new(),
            error: None,
            cached: None,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.error = None;
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, db: &Db) {
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("에이전트")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| self.contents(ui, db));
        if !open {
            self.open = false;
            self.error = None;
        }
    }

    fn contents(&mut self, ui: &mut egui::Ui, db: &Db) {
        let list = match &self.cached {
            Some(list) => list.clone(),
            None => match db.list_agent_configs() {
                Ok(list) => {
                    self.cached = Some(list.clone());
                    list
                }
                Err(e) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("목록 조회 실패: {e:#}"),
                    );
                    return;
                }
            },
        };

        if list.is_empty() {
            ui.label("등록된 에이전트가 없습니다.");
        }
        let mut delete_id = None;
        for config in &list {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} — {} {}",
                    config.name,
                    config.command,
                    config.args.join(" ")
                ));
                if ui.button("삭제").clicked() {
                    delete_id = Some(config.id.clone());
                }
            });
        }
        if let Some(id) = delete_id {
            if let Err(e) = db.delete_agent_config(&id) {
                self.error = Some(format!("{e:#}"));
            } else {
                self.cached = None;
                self.error = None;
            }
        }

        ui.separator();
        ui.heading("등록");
        ui.horizontal(|ui| {
            ui.label("이름");
            ui.text_edit_singleline(&mut self.name);
        });
        ui.horizontal(|ui| {
            ui.label("command");
            ui.text_edit_singleline(&mut self.command);
        });
        ui.label("args (한 줄에 하나)");
        ui.add(
            egui::TextEdit::multiline(&mut self.args_input)
                .desired_rows(3)
                .font(egui::TextStyle::Monospace),
        );
        let filled = !self.name.trim().is_empty() && !self.command.trim().is_empty();
        if ui.add_enabled(filled, egui::Button::new("등록")).clicked() {
            let args: Vec<String> = self
                .args_input
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            match db.insert_agent_config(self.name.trim(), self.command.trim(), &args) {
                Ok(_) => {
                    self.name.clear();
                    self.command.clear();
                    self.args_input.clear();
                    self.cached = None;
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("{e:#}")),
            }
        }

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }
}
