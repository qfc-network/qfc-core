//! Real-time TUI dashboard for QFC inference miner
//!
//! Shows mining stats, earnings, task history, and GPU health in a terminal UI.

#[cfg(feature = "tui")]
pub mod tui {
    use crossterm::{
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
            MouseEventKind,
        },
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
        ExecutableCommand,
    };
    use ratatui::{
        layout::{Constraint, Direction, Layout, Rect},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table},
        Frame, Terminal,
    };
    use std::io::stdout;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};
    use tracing_subscriber::layer::SubscriberExt;

    /// Snapshot of mining stats shared between worker and dashboard
    #[derive(Clone, Debug, Default)]
    pub struct MinerStats {
        /// Miner wallet address
        pub address: String,
        /// Backend name
        pub backend: String,
        /// GPU tier
        pub tier: String,
        /// Validator RPC URL
        pub rpc_url: String,

        /// Tasks completed this session
        pub tasks_completed: u64,
        /// Tasks failed this session
        pub tasks_failed: u64,
        /// Total FLOPS this session
        pub total_flops: u64,

        /// Session earnings in wei
        pub session_earnings_wei: u128,
        /// Current balance in wei (from last RPC query)
        pub balance_wei: u128,

        /// Lifetime earnings in wei (persisted across restarts)
        pub lifetime_earnings_wei: u128,
        /// Lifetime tasks completed (persisted)
        pub lifetime_tasks: u64,
        /// 24h rolling earnings in wei
        pub earnings_24h_wei: u128,

        /// Recent earning amounts in micro-QFC for sparkline
        pub earning_history: Vec<u64>,

        /// Recent task log entries (newest first, max 20)
        pub task_log: Vec<TaskLogEntry>,

        /// Current status line
        pub status: String,
        /// Session start time
        pub session_start: Option<Instant>,

        /// GPU temperature (Celsius, 0 if unavailable)
        pub gpu_temp_c: u32,
        /// GPU utilization percentage (0-100, 0 if unavailable)
        pub gpu_util_pct: u32,
        /// GPU memory usage MB
        pub gpu_mem_used_mb: u64,
        /// GPU memory total MB
        pub gpu_mem_total_mb: u64,

        /// Recent log lines (newest first, max 50)
        pub log_lines: Vec<LogLine>,
    }

    /// A captured log line for TUI display
    #[derive(Clone, Debug)]
    pub struct LogLine {
        pub timestamp: String,
        pub level: String,
        pub message: String,
    }

    /// A single task log entry
    #[derive(Clone, Debug)]
    pub struct TaskLogEntry {
        pub timestamp: String,
        pub task_type: String,
        pub model: String,
        pub duration_ms: u64,
        pub reward_qfc: String,
        pub accepted: bool,
    }

    impl MinerStats {
        fn session_duration(&self) -> Duration {
            self.session_start.map(|s| s.elapsed()).unwrap_or_default()
        }

        fn earnings_per_hour(&self) -> f64 {
            let secs = self.session_duration().as_secs_f64();
            if secs < 1.0 {
                return 0.0;
            }
            let qfc = self.session_earnings_wei as f64 / 1e18;
            qfc / secs * 3600.0
        }
    }

    /// Shared stats handle for worker to update
    pub type SharedStats = Arc<Mutex<MinerStats>>;

    /// Create a new shared stats handle
    pub fn new_shared_stats() -> SharedStats {
        Arc::new(Mutex::new(MinerStats::default()))
    }

    /// Tracing layer that captures log lines into SharedStats for TUI display
    pub struct TuiLogLayer {
        stats: SharedStats,
    }

    impl TuiLogLayer {
        pub fn new(stats: SharedStats) -> Self {
            Self { stats }
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for TuiLogLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let meta = event.metadata();
            let level = meta.level().to_string();

            let mut message = String::new();
            let mut visitor = MessageVisitor(&mut message);
            event.record(&mut visitor);

            let now = format_time_hms();

            if let Ok(mut s) = self.stats.lock() {
                s.log_lines.insert(
                    0,
                    LogLine {
                        timestamp: now,
                        level,
                        message,
                    },
                );
                s.log_lines.truncate(50);
            }
        }
    }

    struct MessageVisitor<'a>(&'a mut String);

    impl<'a> tracing::field::Visit for MessageVisitor<'a> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write;
            if field.name() == "message" {
                let _ = write!(self.0, "{:?}", value);
            } else if !self.0.is_empty() {
                let _ = write!(self.0, " {}={:?}", field.name(), value);
            } else {
                let _ = write!(self.0, "{}={:?}", field.name(), value);
            }
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            use std::fmt::Write;
            if field.name() == "message" {
                let _ = write!(self.0, "{}", value);
            } else if !self.0.is_empty() {
                let _ = write!(self.0, " {}={}", field.name(), value);
            } else {
                let _ = write!(self.0, "{}={}", field.name(), value);
            }
        }
    }

    /// Initialize tracing with TUI log capture + file output.
    /// Logs go to both qfc-miner.log and the in-TUI log panel.
    pub fn init_tui_tracing(stats: SharedStats, verbose: bool) {
        let filter = if verbose { "debug" } else { "info" };
        let log_file = std::fs::File::create("qfc-miner.log").expect("Failed to create log file");
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(log_file)
            .with_ansi(false);
        let tui_layer = TuiLogLayer::new(stats);
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(filter))
            .with(file_layer)
            .with(tui_layer);
        tracing::subscriber::set_global_default(subscriber)
            .expect("Failed to set tracing subscriber");
    }

    /// Run the TUI dashboard (blocking). Call from a spawned task.
    /// Returns when user presses 'q' or Ctrl+C.
    pub fn run_dashboard(stats: SharedStats) -> std::io::Result<()> {
        enable_raw_mode()?;
        stdout().execute(EnterAlternateScreen)?;
        stdout().execute(EnableMouseCapture)?;
        let backend = ratatui::backend::CrosstermBackend::new(stdout());
        let mut terminal = Terminal::new(backend)?;

        let tick_rate = Duration::from_millis(250);
        let mut last_tick = Instant::now();
        let mut log_scroll: usize = 0;
        let mut log_follow = true; // auto-scroll to newest
        let mut log_pct: u16 = 60; // log panel percentage of body area
        let mut log_visible = true;
        let mut task_log_scroll: usize = 0;
        // Track panel areas for mouse-aware scrolling
        let mut task_log_area = Rect::default();
        let mut log_panel_area = Rect::default();

        loop {
            let total_logs = stats.lock().unwrap().log_lines.len();
            let total_tasks = stats.lock().unwrap().task_log.len();
            let effective_log_pct = if log_visible { log_pct } else { 0 };

            terminal.draw(|f| {
                let s = stats.lock().unwrap();
                let areas =
                    draw_ui(f, &s, log_scroll, task_log_scroll, effective_log_pct);
                task_log_area = areas.0;
                log_panel_area = areas.1;
            })?;

            let timeout = tick_rate.saturating_sub(last_tick.elapsed());
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => break,
                        // Toggle log panel
                        KeyCode::Char('l') | KeyCode::Char('L') => {
                            log_visible = !log_visible;
                        }
                        // Resize log panel (±5% each step, 20%–80%)
                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            if log_visible {
                                log_pct = log_pct.saturating_add(5).min(80);
                            }
                        }
                        KeyCode::Char('-') | KeyCode::Char('_') => {
                            if log_visible {
                                log_pct = log_pct.saturating_sub(5).max(20);
                            }
                        }
                        // Log scroll (keyboard)
                        KeyCode::Up | KeyCode::Char('k') => {
                            if log_visible && log_scroll + 1 < total_logs {
                                log_scroll += 1;
                                log_follow = false;
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if log_visible && log_scroll > 0 {
                                log_scroll -= 1;
                            }
                            if log_scroll == 0 {
                                log_follow = true;
                            }
                        }
                        KeyCode::PageUp => {
                            if log_visible {
                                log_scroll = (log_scroll + 5).min(total_logs.saturating_sub(1));
                                log_follow = false;
                            }
                        }
                        KeyCode::PageDown => {
                            if log_visible {
                                log_scroll = log_scroll.saturating_sub(5);
                                if log_scroll == 0 {
                                    log_follow = true;
                                }
                            }
                        }
                        KeyCode::Home => {
                            if log_visible {
                                log_scroll = total_logs.saturating_sub(1);
                                log_follow = false;
                            }
                        }
                        KeyCode::End => {
                            log_scroll = 0;
                            log_follow = true;
                        }
                        _ => {}
                    },
                    // Mouse scroll — region-aware
                    Event::Mouse(mouse) => {
                        let col = mouse.column;
                        let row = mouse.row;
                        let in_task_log = col >= task_log_area.x
                            && col < task_log_area.x + task_log_area.width
                            && row >= task_log_area.y
                            && row < task_log_area.y + task_log_area.height;
                        let in_log_panel = col >= log_panel_area.x
                            && col < log_panel_area.x + log_panel_area.width
                            && row >= log_panel_area.y
                            && row < log_panel_area.y + log_panel_area.height;

                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                if in_task_log {
                                    task_log_scroll =
                                        task_log_scroll.saturating_add(1).min(total_tasks.saturating_sub(1));
                                } else if in_log_panel && log_visible {
                                    log_scroll = log_scroll
                                        .saturating_add(1)
                                        .min(total_logs.saturating_sub(1));
                                    log_follow = false;
                                }
                            }
                            MouseEventKind::ScrollDown => {
                                if in_task_log {
                                    task_log_scroll = task_log_scroll.saturating_sub(1);
                                } else if in_log_panel && log_visible {
                                    log_scroll = log_scroll.saturating_sub(1);
                                    if log_scroll == 0 {
                                        log_follow = true;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }

            // Auto-follow: reset scroll when new logs arrive
            if log_follow {
                log_scroll = 0;
            }

            if last_tick.elapsed() >= tick_rate {
                last_tick = Instant::now();
            }
        }

        disable_raw_mode()?;
        stdout().execute(DisableMouseCapture)?;
        stdout().execute(LeaveAlternateScreen)?;
        Ok(())
    }

    /// Draw the full UI. Returns (task_log_area, log_panel_area) for mouse hit-testing.
    /// `log_pct` is the percentage of body area for the log panel (0 = collapsed).
    fn draw_ui(
        f: &mut Frame,
        stats: &MinerStats,
        log_scroll: usize,
        task_log_scroll: usize,
        log_pct: u16,
    ) -> (Rect, Rect) {
        let size = f.area();

        // Main layout: header, body, footer
        let main_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Header
                Constraint::Min(10),   // Body
                Constraint::Length(3), // Footer
            ])
            .split(size);

        draw_header(f, main_chunks[0], stats);
        let areas = draw_body(f, main_chunks[1], stats, log_scroll, task_log_scroll, log_pct);
        draw_footer(f, main_chunks[2], stats, log_pct > 0);
        areas
    }

    fn draw_header(f: &mut Frame, area: Rect, stats: &MinerStats) {
        let duration = stats.session_duration();
        let hours = duration.as_secs() / 3600;
        let mins = (duration.as_secs() % 3600) / 60;
        let secs = duration.as_secs() % 60;

        let header_text = Line::from(vec![
            Span::styled(
                " QFC INFERENCE MINER ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{}", stats.address),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{}  T{}", stats.backend, stats.tier),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{:02}:{:02}:{:02}", hours, mins, secs),
                Style::default().fg(Color::White),
            ),
        ]);

        let header = Paragraph::new(header_text).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(header, area);
    }

    /// Draw the body. Returns (task_log_area, log_panel_area).
    /// `log_pct` is the percentage of body area for the log panel (0 = collapsed).
    fn draw_body(
        f: &mut Frame,
        area: Rect,
        stats: &MinerStats,
        log_scroll: usize,
        task_log_scroll: usize,
        log_pct: u16,
    ) -> (Rect, Rect) {
        // Body: upper (stats + task log) | lower (logs, if visible)
        let constraints = if log_pct > 0 {
            vec![
                Constraint::Percentage(100 - log_pct),
                Constraint::Percentage(log_pct),
            ]
        } else {
            vec![Constraint::Min(10)]
        };
        let vert_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        // Upper: left (stats + earnings) | right (task log)
        let body_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(vert_chunks[0]);

        // Left: stats panels stacked
        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(9), // Earnings
                Constraint::Length(6), // Performance
                Constraint::Min(4),    // GPU / Sparkline
            ])
            .split(body_chunks[0]);

        draw_earnings_panel(f, left_chunks[0], stats);
        draw_performance_panel(f, left_chunks[1], stats);
        draw_gpu_panel(f, left_chunks[2], stats);

        // Right: task log
        let task_log_rect = body_chunks[1];
        draw_task_log(f, task_log_rect, stats, task_log_scroll);

        // Lower: log panel (if visible)
        let log_panel_rect = if log_pct > 0 && vert_chunks.len() > 1 {
            let r = vert_chunks[1];
            draw_log_panel(f, r, stats, log_scroll);
            r
        } else {
            Rect::default()
        };

        (task_log_rect, log_panel_rect)
    }

    fn draw_earnings_panel(f: &mut Frame, area: Rect, stats: &MinerStats) {
        let session_qfc = stats.session_earnings_wei as f64 / 1e18;
        let _balance_qfc = stats.balance_wei as f64 / 1e18;
        let per_hour = stats.earnings_per_hour();
        let lifetime_qfc = stats.lifetime_earnings_wei as f64 / 1e18;
        let earnings_24h_qfc = stats.earnings_24h_wei as f64 / 1e18;

        let text = vec![
            Line::from(vec![
                Span::styled("  Session:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{:.6} QFC", session_qfc),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Rate:     ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{:.6} QFC/hr", per_hour),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Lifetime: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{:.6} QFC ({} tasks)", lifetime_qfc, stats.lifetime_tasks),
                    Style::default().fg(Color::Cyan),
                ),
            ]),
            Line::from(vec![
                Span::styled("  24h:      ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{:.6} QFC", earnings_24h_qfc),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(""),
        ];

        let block = Block::default()
            .title(Span::styled(
                " Earnings ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let paragraph = Paragraph::new(text).block(block);
        f.render_widget(paragraph, area);
    }

    fn draw_performance_panel(f: &mut Frame, area: Rect, stats: &MinerStats) {
        let total = stats.tasks_completed + stats.tasks_failed;
        let success_rate = if total > 0 {
            stats.tasks_completed as f64 / total as f64 * 100.0
        } else {
            0.0
        };

        let text = vec![
            Line::from(vec![
                Span::styled("  Completed: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}", stats.tasks_completed),
                    Style::default().fg(Color::Green),
                ),
                Span::styled("  Failed: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}", stats.tasks_failed),
                    Style::default().fg(if stats.tasks_failed > 0 {
                        Color::Red
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(
                    format!("  ({:.0}%)", success_rate),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(vec![
                Span::styled("  FLOPS:     ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format_flops(stats.total_flops),
                    Style::default().fg(Color::White),
                ),
            ]),
        ];

        let block = Block::default()
            .title(Span::styled(
                " Performance ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let paragraph = Paragraph::new(text).block(block);
        f.render_widget(paragraph, area);
    }

    fn draw_gpu_panel(f: &mut Frame, area: Rect, stats: &MinerStats) {
        // Sparkline of recent earnings
        let block = Block::default()
            .title(Span::styled(
                " Earnings History ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        if !stats.earning_history.is_empty() {
            let sparkline = Sparkline::default()
                .block(block)
                .data(&stats.earning_history)
                .style(Style::default().fg(Color::Green));
            f.render_widget(sparkline, area);
        } else {
            let paragraph = Paragraph::new("  Waiting for tasks...")
                .block(block)
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(paragraph, area);
        }
    }

    fn draw_task_log(f: &mut Frame, area: Rect, stats: &MinerStats, scroll: usize) {
        let total = stats.task_log.len();
        let title = if scroll > 0 {
            format!(" Task Log [{}/{}] ", scroll, total)
        } else {
            " Task Log ".to_string()
        };

        let header_cells = ["Time", "Type", "Model", "Duration", "Reward"]
            .iter()
            .map(|h| {
                Cell::from(*h).style(
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
            });
        let header = Row::new(header_cells).height(1);

        let rows = stats.task_log.iter().skip(scroll).map(|entry| {
            let color = if entry.accepted {
                Color::Green
            } else {
                Color::Red
            };
            let cells = vec![
                Cell::from(entry.timestamp.clone()).style(Style::default().fg(Color::DarkGray)),
                Cell::from(entry.task_type.clone()).style(Style::default().fg(Color::White)),
                Cell::from(truncate_str(&entry.model, 16)).style(Style::default().fg(Color::Cyan)),
                Cell::from(format!("{}ms", entry.duration_ms))
                    .style(Style::default().fg(Color::Yellow)),
                Cell::from(format!("+{}", entry.reward_qfc)).style(Style::default().fg(color)),
            ];
            Row::new(cells)
        });

        let table = Table::new(
            rows,
            [
                Constraint::Length(8),
                Constraint::Length(10),
                Constraint::Length(16),
                Constraint::Length(8),
                Constraint::Min(12),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

        f.render_widget(table, area);
    }

    fn draw_footer(f: &mut Frame, area: Rect, stats: &MinerStats, log_visible: bool) {
        let log_hint = if log_visible { "l:hide logs" } else { "l:show logs" };
        let footer_text = Line::from(vec![
            Span::styled(" Status: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&stats.status, Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled(
                format!("q:quit  {}  +/-:resize  scroll:mouse/↑↓", log_hint),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        let footer = Paragraph::new(footer_text).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(footer, area);
    }

    fn format_flops(flops: u64) -> String {
        if flops >= 1_000_000_000 {
            format!("{:.2} GFLOPS", flops as f64 / 1e9)
        } else if flops >= 1_000_000 {
            format!("{:.2} MFLOPS", flops as f64 / 1e6)
        } else if flops >= 1_000 {
            format!("{:.1} KFLOPS", flops as f64 / 1e3)
        } else {
            format!("{} FLOPS", flops)
        }
    }

    fn truncate_str(s: &str, max_len: usize) -> String {
        if s.len() > max_len {
            format!("{}...", &s[..max_len - 3])
        } else {
            s.to_string()
        }
    }

    /// Format current UTC time as HH:MM:SS without external dependencies
    fn format_time_hms() -> String {
        let dur = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let total_secs = dur.as_secs();
        let h = (total_secs / 3600) % 24;
        let m = (total_secs % 3600) / 60;
        let s = total_secs % 60;
        format!("{:02}:{:02}:{:02}", h, m, s)
    }

    fn draw_log_panel(f: &mut Frame, area: Rect, stats: &MinerStats, scroll: usize) {
        let total = stats.log_lines.len();
        let title = if scroll > 0 {
            format!(" Logs [{}/{}] ↑↓ scroll, End=latest ", scroll, total)
        } else {
            " Logs [live] ↑↓/PgUp/PgDn scroll ".to_string()
        };

        let block = Block::default()
            .title(Span::styled(
                title,
                Style::default()
                    .fg(if scroll > 0 {
                        Color::Yellow
                    } else {
                        Color::Blue
                    })
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = block.inner(area);
        f.render_widget(block, area);

        let max_lines = inner.height as usize;
        let lines: Vec<Line> = stats
            .log_lines
            .iter()
            .skip(scroll)
            .take(max_lines)
            .map(|log| {
                let level_color = match log.level.as_str() {
                    "ERROR" => Color::Red,
                    "WARN" => Color::Yellow,
                    "INFO" => Color::Green,
                    "DEBUG" => Color::Cyan,
                    _ => Color::DarkGray,
                };
                Line::from(vec![
                    Span::styled(
                        format!("{} ", log.timestamp),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!("{:<5} ", log.level),
                        Style::default().fg(level_color),
                    ),
                    Span::styled(&log.message, Style::default().fg(Color::White)),
                ])
            })
            .collect();

        let paragraph = Paragraph::new(lines);
        f.render_widget(paragraph, inner);
    }
}

// Non-TUI fallback: stats are still tracked but no UI
#[cfg(not(feature = "tui"))]
pub mod tui {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    #[derive(Clone, Debug, Default)]
    pub struct MinerStats {
        pub address: String,
        pub backend: String,
        pub tier: String,
        pub rpc_url: String,
        pub tasks_completed: u64,
        pub tasks_failed: u64,
        pub total_flops: u64,
        pub session_earnings_wei: u128,
        pub balance_wei: u128,
        pub lifetime_earnings_wei: u128,
        pub lifetime_tasks: u64,
        pub earnings_24h_wei: u128,
        pub earning_history: Vec<u64>,
        pub task_log: Vec<TaskLogEntry>,
        pub status: String,
        pub session_start: Option<Instant>,
        pub gpu_temp_c: u32,
        pub gpu_util_pct: u32,
        pub gpu_mem_used_mb: u64,
        pub gpu_mem_total_mb: u64,
        pub log_lines: Vec<LogLine>,
    }

    #[derive(Clone, Debug)]
    pub struct LogLine {
        pub timestamp: String,
        pub level: String,
        pub message: String,
    }

    #[derive(Clone, Debug)]
    pub struct TaskLogEntry {
        pub timestamp: String,
        pub task_type: String,
        pub model: String,
        pub duration_ms: u64,
        pub reward_qfc: String,
        pub accepted: bool,
    }

    pub type SharedStats = Arc<Mutex<MinerStats>>;

    pub fn new_shared_stats() -> SharedStats {
        Arc::new(Mutex::new(MinerStats::default()))
    }
}
