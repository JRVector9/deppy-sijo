use std::time::Instant;
const SOURCE_EDITOR_LEFT_MARGIN: i8 = 12;
const SOURCE_EDITOR_LINE_HEIGHT: f32 = 1.75;
pub fn source_editor(ui: &mut egui::Ui, id: egui::Id, source: &mut String, editable: bool) -> bool {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // 원문은 **고정폭**이어야 한다 — `TextEdit`의 기본은 비례 폰트라 표·
            // 들여쓰기·코드 펜스가 어긋나 보였다(2026-08-22 사용자 스크린샷).
            let font = egui::TextStyle::Monospace.resolve(ui.style());
            let row = ui.fonts_mut(|fonts| fonts.row_height(&font));
            ui.add(
                egui::TextEdit::multiline(source)
                    .id(id)
                    .interactive(editable)
                    .font(font.clone())
                    // 배경·테두리 없는 프레임에 **여백만** 싣는다.
                    //
                    // `.margin(..)`은 여기서 쓸 수 없다 — egui는 프레임을 명시하면
                    // margin을 버린다(`builder.rs`: `frame.unwrap_or_else(|| ...
                    // .inner_margin(margin))`). 그래서 margin만 주면 여백이 통째로
                    // 무시돼 첫 글자가 경계선에 붙는다(2026-08-22 실측).
                    //
                    // 컨테이너를 하나 더 끼우는 방법은 쓰지 않는다 — 위젯 계층이
                    // 바뀌어 `app.rs`의 `document_source_editor_state_id` 공식이
                    // 어긋난다(문서를 닫을 때 undo 기록을 지우는 그 id다).
                    .frame(egui::Frame::NONE.inner_margin(egui::Margin {
                        left: SOURCE_EDITOR_LEFT_MARGIN,
                        right: 0,
                        top: 2,
                        bottom: 0,
                    }))
                    .desired_width(f32::INFINITY)
                    .layouter(&mut |ui, text, wrap_width| {
                        let mut job = egui::text::LayoutJob::simple(
                            text.as_str().to_owned(),
                            font.clone(),
                            ui.visuals().text_color(),
                            wrap_width,
                        );
                        // 줄 간격은 섹션 단위로만 줄 수 있다 — `TextEdit`은 우리가
                        // job을 만들어 넘기므로 여기서 지정한다.
                        for section in &mut job.sections {
                            section.format.line_height = Some(row * SOURCE_EDITOR_LINE_HEIGHT);
                        }
                        ui.fonts_mut(|fonts| fonts.layout_job(job))
                    }),
            )
        })
        .inner
        .changed()
}

fn main() {
    let ctx=egui::Context::default();
    let id=egui::Id::new("production-document-editor");
    let mut source=String::new();
    let run=|ctx: &egui::Context, source: &mut String, events| {
        let mut output=ctx.run_ui(egui::RawInput { screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(900.0,700.0))), events, ..Default::default() }, |ui| {
            source_editor(ui,id,source,true);
        });
        output.textures_delta.clear();
    };
    let _=run(&ctx,&mut source,vec![]);
    ctx.memory_mut(|memory|memory.request_focus(id));
    let payload="a\n".repeat(9*1024*1024/2);
    let start=Instant::now();
    let _=run(&ctx,&mut source,vec![egui::Event::Paste(payload)]);
    let elapsed=start.elapsed();
    println!("production editor retained={} bytes; paste/render elapsed={elapsed:?}; refuse_limit={} bytes",source.len(),8*1024*1024);
    assert!(source.len()<=8*1024*1024,"editable document exceeded absolute save/load byte limit");
}
