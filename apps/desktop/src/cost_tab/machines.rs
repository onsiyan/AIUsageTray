//! Other machines whose Codex and Claude Code logs the Cost page counts, read
//! over SSH: the list, the form that adds one, and each one's last reading.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use usage_monitor_core::cost::{
    self,
    remote::{self, HostProblem, RemoteError, RemoteOs, SshHost},
};

use super::*;
// Explicit, so it is not confused with the built-in column! macro.
use iced::widget::column;

/// How often an open Cost page reads the machines again.
pub(super) const LIVE_SYNC: Duration = Duration::from_secs(60);
/// How often they are read while the page is closed.
pub(super) const BACKGROUND_SYNC: Duration = Duration::from_secs(15 * 60);
const NAME_INPUT: &str = "cost-machine-name";

#[derive(Default)]
pub(super) struct Machines {
    hosts: Vec<SshHost>,
    status: BTreeMap<String, Status>,
    syncing: bool,
    /// A machine was added while a read was running: read again after it.
    sync_again: bool,
    last_sync: Option<Instant>,
    form: Option<HostForm>,
    /// `Host` names from `~/.ssh/config`, offered in the form.
    config_hosts: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct Status {
    synced_at: Option<DateTime<Utc>>,
    error: Option<RemoteError>,
    reading: bool,
}

#[derive(Debug, Clone)]
struct HostForm {
    name: String,
    target: String,
    os: RemoteOs,
    problem: Option<HostProblem>,
}

/// Which machines the page counts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Scope {
    #[default]
    All,
    ThisPc,
    Machine(String),
}

impl Scope {
    /// The scope of one machine of a report (`None` is this PC).
    pub(super) fn of(name: Option<&str>) -> Self {
        name.map_or(Self::ThisPc, |name| Self::Machine(name.to_owned()))
    }

    /// The machine's name in a report, or `None` for every machine.
    pub(super) fn machine(&self) -> Option<Option<&str>> {
        match self {
            Self::All => None,
            Self::ThisPc => Some(None),
            Self::Machine(name) => Some(Some(name)),
        }
    }

    pub(super) fn label(&self, language: locale::Language) -> String {
        match self {
            Self::All => tr(language, "All machines", "كل الأجهزة").to_owned(),
            Self::ThisPc => tr(language, "This PC", "هذا الجهاز").to_owned(),
            Self::Machine(name) => name.clone(),
        }
    }
}

/// A choice in the machine picker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScopeChoice {
    scope: Scope,
    label: String,
}

impl std::fmt::Display for ScopeChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.label)
    }
}

/// The machine picker beside the period, once there is more than this PC.
pub(super) fn picker(
    report: &CostReport,
    scope: &Scope,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Option<Element<'static, Message>> {
    if report.machines.is_empty() {
        return None;
    }
    let choices = std::iter::once(Scope::All)
        .chain(
            report
                .machines
                .iter()
                .map(|machine| Scope::of(machine.name.as_deref())),
        )
        .map(|scope| ScopeChoice {
            label: scope.label(language),
            scope,
        })
        .collect::<Vec<_>>();
    let selected = choices
        .iter()
        .find(|choice| choice.scope == *scope)
        .cloned();
    Some(super::dropdown(
        choices,
        selected,
        |choice: ScopeChoice| Message::CostView(CostView::Scope(choice.scope)),
        scope != &Scope::All,
        theme,
    ))
}

/// What a machine read returned, by machine name.
pub(crate) type SyncResults = Vec<(String, Result<DateTime<Utc>, RemoteError>)>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MachineChange {
    OpenForm,
    CloseForm,
    Name(String),
    Target(String),
    /// A `Host` from `~/.ssh/config`: the target, and the name if empty.
    PickConfigHost(String),
    Os(RemoteOs),
    Save,
    Remove(String),
}

impl Machines {
    pub(super) fn load() -> Self {
        let hosts = crate::theme::preference_directory()
            .map(|directory| remote::load_hosts(&directory))
            .unwrap_or_default();
        let cache_directory = cost::default_cache_directory();
        let status = hosts
            .iter()
            .map(|host| {
                let synced_at =
                    remote::stored(&cache_directory, &host.name).map(|reading| reading.synced_at);
                (
                    host.name.clone(),
                    Status {
                        synced_at,
                        ..Status::default()
                    },
                )
            })
            .collect();
        Self {
            hosts,
            status,
            ..Self::default()
        }
    }

    pub(super) fn names(&self) -> Vec<String> {
        self.hosts.iter().map(|host| host.name.clone()).collect()
    }

    pub(super) fn sync_if_due(&mut self, every: Duration) -> Task<Message> {
        let due = self.last_sync.is_none_or(|last| last.elapsed() >= every);
        if due { self.sync() } else { Task::none() }
    }

    /// Reads every machine at once, each on its own thread.
    pub(super) fn sync(&mut self) -> Task<Message> {
        if self.hosts.is_empty() {
            return Task::none();
        }
        if self.syncing {
            self.sync_again = true;
            return Task::none();
        }
        self.syncing = true;
        self.last_sync = Some(Instant::now());
        for host in &self.hosts {
            self.status.entry(host.name.clone()).or_default().reading = true;
        }
        let hosts = self.hosts.clone();
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let cache_directory = cost::default_cache_directory();
            let results = std::thread::scope(|scope| {
                let reads = hosts
                    .iter()
                    .map(|host| {
                        let cache_directory = &cache_directory;
                        scope.spawn(move || {
                            let known = remote::stored(cache_directory, &host.name);
                            let result = remote::fetch(host, known.as_ref(), remote::SYNC_TIMEOUT)
                                .map(|reading| {
                                    if let Err(error) =
                                        remote::store(cache_directory, &host.name, &reading)
                                    {
                                        preview_log(format!(
                                            "keeping {}'s reading failed: {error}",
                                            host.name
                                        ));
                                    }
                                    reading.synced_at
                                });
                            (host.name.clone(), result)
                        })
                    })
                    .collect::<Vec<_>>();
                reads
                    .into_iter()
                    .filter_map(|read| read.join().ok())
                    .collect::<SyncResults>()
            });
            let _ = sender.send_blocking(results);
        });
        Task::perform(
            async move { receiver.recv().await.unwrap_or_default() },
            Message::CostMachinesSynced,
        )
    }

    /// Notes each machine's result. Returns whether to read them again now.
    pub(super) fn finish_sync(&mut self, results: SyncResults) -> bool {
        self.syncing = false;
        for (name, result) in results {
            let Some(status) = self.status.get_mut(&name) else {
                continue;
            };
            status.reading = false;
            match result {
                Ok(synced_at) => {
                    status.synced_at = Some(synced_at);
                    status.error = None;
                }
                Err(error) => {
                    preview_log(format!("reading {name} failed: {error:?}"));
                    status.error = Some(error);
                }
            }
        }
        std::mem::take(&mut self.sync_again)
    }

    /// Applies a change from the page. Returns a task to run and whether
    /// the report must be built again.
    pub(super) fn change(&mut self, change: MachineChange) -> (Task<Message>, bool) {
        match change {
            MachineChange::OpenForm => {
                self.config_hosts = remote::ssh_config_hosts()
                    .into_iter()
                    .filter(|name| !self.hosts.iter().any(|host| host.target == *name))
                    .collect();
                self.form = Some(HostForm {
                    name: String::new(),
                    target: String::new(),
                    os: RemoteOs::Unix,
                    problem: None,
                });
                return (iced::widget::operation::focus(NAME_INPUT), false);
            }
            MachineChange::CloseForm => self.form = None,
            MachineChange::Name(name) => {
                if let Some(form) = &mut self.form {
                    form.name = name;
                    form.problem = None;
                }
            }
            MachineChange::Target(target) => {
                if let Some(form) = &mut self.form {
                    form.target = target;
                    form.problem = None;
                }
            }
            MachineChange::PickConfigHost(target) => {
                if let Some(form) = &mut self.form {
                    if form.name.trim().is_empty() {
                        form.name = target.clone();
                    }
                    form.target = target;
                    form.problem = None;
                }
            }
            MachineChange::Os(os) => {
                if let Some(form) = &mut self.form {
                    form.os = os;
                }
            }
            MachineChange::Save => {
                let Some(form) = &mut self.form else {
                    return (Task::none(), false);
                };
                match SshHost::new(&form.name, &form.target, form.os, &self.hosts) {
                    Ok(host) => {
                        self.status.insert(host.name.clone(), Status::default());
                        self.hosts.push(host);
                        self.form = None;
                        self.save();
                        return (self.sync(), false);
                    }
                    Err(problem) => form.problem = Some(problem),
                }
            }
            MachineChange::Remove(name) => {
                self.hosts.retain(|host| host.name != name);
                self.status.remove(&name);
                remote::forget(&cost::default_cache_directory(), &name);
                self.save();
                return (Task::none(), true);
            }
        }
        (Task::none(), false)
    }

    fn save(&self) {
        let saved = crate::theme::preference_directory()
            .and_then(|directory| remote::save_hosts(&directory, &self.hosts));
        if let Err(error) = saved {
            preview_log(format!("saving machines failed: {error}"));
        }
    }
}

fn change(change: MachineChange) -> Message {
    Message::CostView(CostView::Machine(change))
}

/// The machines and what each one's use came to over `days` days. A machine
/// is picked by clicking it, like in the picker.
pub(super) fn section(
    machines: &Machines,
    report: &CostReport,
    days: usize,
    scope: &Scope,
    hovered: Option<&Scope>,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let english = language == locale::Language::English;
    let mut title = row![
        text(tr(language, "Machines", "الأجهزة"))
            .size(typography::LABEL_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text())
            .width(Fill),
    ]
    .align_y(Alignment::Center);
    if machines.form.is_none() {
        title = title.push(plan_button(
            tr(language, "+ Add machine", "+ إضافة جهاز"),
            false,
            CostView::Machine(MachineChange::OpenForm),
            theme,
        ));
    }
    let mut rows = column![title].spacing(6);

    let this_pc = report
        .machines
        .iter()
        .find(|machine| machine.name.is_none())
        .map_or_else(
            || {
                report
                    .tools
                    .iter()
                    .map(|tool| tool.last(days).cost_usd)
                    .sum()
            },
            |machine| machine.cost_over(days),
        );
    // Picking needs another machine to pick from.
    let pickable = !report.machines.is_empty();
    rows = rows.push(machine_row(
        MachineRow {
            icon: icon_monitor::<Theme>()
                .size(15)
                .color(theme.colors.text())
                .into(),
            name: Scope::ThisPc.label(language),
            status: None,
            amount: Some(this_pc),
            scope: Scope::ThisPc,
            removable: false,
        },
        pickable.then_some(scope),
        hovered,
        theme,
    ));

    let now = Utc::now();
    for host in &machines.hosts {
        let status = machines.status.get(&host.name).cloned().unwrap_or_default();
        let amount = report
            .machines
            .iter()
            .find(|machine| machine.name.as_deref() == Some(host.name.as_str()))
            .map(|machine| machine.cost_over(days));
        let (line, failed) = status_line(&status, now, language);
        rows = rows.push(machine_row(
            MachineRow {
                icon: icon_server::<Theme>()
                    .size(15)
                    .color(theme.colors.text())
                    .into(),
                name: host.name.clone(),
                status: Some((line, failed)),
                amount,
                scope: Scope::Machine(host.name.clone()),
                removable: true,
            },
            (pickable && amount.is_some()).then_some(scope),
            hovered,
            theme,
        ));
    }

    if let Some(form) = &machines.form {
        rows = rows.push(host_form(machines, form, theme, language));
    } else {
        rows = rows.push(muted_line(
            if english {
                "Counts Codex and Claude Code on your other machines over SSH, with your keys; no password is asked or kept."
            } else {
                "يحسب Codex وClaude Code على أجهزتك الأخرى عبر SSH بمفاتيحك، دون أن يطلب كلمة مرور أو يحفظها."
            },
            theme,
        ));
    }
    rows.into()
}

struct MachineRow {
    icon: Element<'static, Message>,
    name: String,
    status: Option<(String, bool)>,
    amount: Option<f64>,
    scope: Scope,
    removable: bool,
}

/// Width kept before every row's amount for the remove button, so a name
/// does not move when the button shows.
const REMOVE_SLOT: f32 = 22.0;

/// One machine: its name and state, its amount, and, under the pointer, a
/// remove button. `picked` is the page's scope when rows can be picked.
fn machine_row(
    machine: MachineRow,
    picked: Option<&Scope>,
    hovered: Option<&Scope>,
    theme: &'static ThemeDefinition,
) -> Element<'static, Message> {
    let MachineRow {
        icon,
        name,
        status,
        amount,
        scope,
        removable,
    } = machine;
    let is_hovered = hovered == Some(&scope);
    let is_picked = picked == Some(&scope);
    let pickable = picked.is_some();
    let mut label = column![
        text(name.clone())
            .size(typography::LABEL_SIZE)
            .font(if is_picked {
                typography::EMPHASIS
            } else {
                typography::MEDIUM
            })
            .color(theme.colors.text())
    ];
    if let Some((line, failed)) = status {
        label = label.push(
            text(line)
                .size(typography::COMPACT_SIZE)
                .font(typography::MEDIUM)
                .color(if failed {
                    Color::from_rgb8(0xE0, 0x6C, 0x5F)
                } else {
                    theme.colors.muted_text()
                }),
        );
    }
    let remove: Element<'static, Message> = if removable && is_hovered {
        button(container(icon_x::<Theme>().size(13).color(theme.colors.muted_text())).center(Fill))
            .on_press(change(MachineChange::Remove(name)))
            .width(REMOVE_SLOT)
            .height(REMOVE_SLOT)
            .padding(0)
            .style(move |framework_theme: &Theme, status| {
                let mut style = button::text(framework_theme, status);
                style.background =
                    matches!(status, button::Status::Hovered | button::Status::Pressed)
                        .then(|| Background::Color(theme.colors.hover()));
                style.border = Border {
                    radius: 6.0.into(),
                    ..Border::default()
                };
                style
            })
            .into()
    } else {
        Space::new().width(REMOVE_SLOT).height(REMOVE_SLOT).into()
    };
    let line = row![
        container(icon).width(18).center_x(18),
        label.width(Fill),
        remove,
        text(amount.map_or_else(|| "–".to_owned(), format_dollars))
            .size(typography::LABEL_SIZE)
            .font(typography::EMPHASIS)
            .color(theme.colors.text()),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let area = container(line)
        .padding([4, 6])
        .width(Fill)
        .style(move |_| container::Style {
            background: (is_picked || (is_hovered && pickable)).then(|| {
                let hover = theme.colors.hover();
                Background::Color(if is_picked {
                    hover
                } else {
                    hover.scale_alpha(0.5)
                })
            }),
            border: Border {
                radius: 7.0.into(),
                ..Border::default()
            },
            ..Default::default()
        });
    let mut area = mouse_area(area)
        .on_enter(Message::CostView(CostView::HoverMachine(Some(
            scope.clone(),
        ))))
        .on_exit(Message::CostView(CostView::HoverMachine(None)));
    if pickable {
        // Clicking the picked machine shows every machine again.
        let next = if is_picked { Scope::All } else { scope };
        area = area
            .on_press(Message::CostView(CostView::Scope(next)))
            .interaction(iced::mouse::Interaction::Pointer);
    }
    area.into()
}

/// When the machine was last read, or why it could not be.
fn status_line(status: &Status, now: DateTime<Utc>, language: locale::Language) -> (String, bool) {
    if status.reading {
        let reading = if status.synced_at.is_none() {
            tr(
                language,
                "Reading… the first read can take a few minutes",
                "جارٍ القراءة… القراءة الأولى قد تأخذ دقائق",
            )
        } else {
            tr(language, "Reading…", "جارٍ القراءة…")
        };
        return (reading.to_owned(), false);
    }
    if let Some(error) = &status.error {
        return (error_text(error, language), true);
    }
    match status.synced_at {
        Some(synced_at) => (synced_ago(now - synced_at, language), false),
        None => (
            tr(language, "Not read yet", "لم يُقرأ بعد").to_owned(),
            false,
        ),
    }
}

fn synced_ago(elapsed: chrono::TimeDelta, language: locale::Language) -> String {
    let minutes = elapsed.num_minutes().max(0);
    let english = language == locale::Language::English;
    match (minutes, english) {
        (0, true) => "Synced just now".to_owned(),
        (0, false) => "قُرئ الآن".to_owned(),
        (1..60, true) => format!("Synced {minutes} min ago"),
        (1..60, false) => format!("قُرئ قبل {minutes} د"),
        (_, true) => format!("Synced {} h ago", minutes / 60),
        (_, false) => format!("قُرئ قبل {} س", minutes / 60),
    }
}

fn error_text(error: &RemoteError, language: locale::Language) -> String {
    let english = language == locale::Language::English;
    let text = match (error, english) {
        (RemoteError::NoSsh, true) => "OpenSSH client is not installed on this PC",
        (RemoteError::NoSsh, false) => "عميل OpenSSH غير مثبّت على هذا الجهاز",
        (RemoteError::SignIn, true) => "The machine did not accept your SSH key",
        (RemoteError::SignIn, false) => "الجهاز لم يقبل مفتاح SSH الخاص بك",
        (RemoteError::HostKey, true) => "Host key not trusted yet: connect once from a terminal",
        (RemoteError::HostKey, false) => "مفتاح الجهاز غير موثوق بعد: اتصل به مرة من الطرفية",
        (RemoteError::Unreachable, true) => "Can't reach the machine",
        (RemoteError::Unreachable, false) => "تعذّر الوصول إلى الجهاز",
        (RemoteError::NoPython, true) => "python3 is not installed there",
        (RemoteError::NoPython, false) => "python3 غير مثبّت على الجهاز",
        (RemoteError::TimedOut, true) => "The read took too long",
        (RemoteError::TimedOut, false) => "استغرقت القراءة وقتًا طويلًا",
        (RemoteError::Failed(detail), _) => return detail.clone(),
    };
    text.to_owned()
}

fn host_form(
    machines: &Machines,
    form: &HostForm,
    theme: &'static ThemeDefinition,
    language: locale::Language,
) -> Element<'static, Message> {
    let input = |placeholder: &'static str, value: &str| {
        text_input(placeholder, value)
            .size(typography::LABEL_SIZE)
            .padding([5, 8])
            .width(Fill)
            .style(move |framework_theme, status| {
                crate::dialogs::account_key_input_style(framework_theme, status, theme)
            })
    };
    let label = |english: &'static str, arabic: &'static str| {
        text(tr(language, english, arabic))
            .size(typography::METADATA_SIZE)
            .font(typography::MEDIUM)
            .color(theme.colors.muted_text())
            .width(70)
    };
    let mut fields = column![
        row![
            label("Name", "الاسم"),
            input("vps", &form.name)
                .id(NAME_INPUT)
                .on_input(|name| change(MachineChange::Name(name)))
                .on_submit(change(MachineChange::Save)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        row![
            label("SSH", "SSH"),
            input(
                tr(
                    language,
                    "user@host, or a Host from ~/.ssh/config",
                    "user@host أو Host من ~/.ssh/config"
                ),
                &form.target
            )
            .on_input(|target| change(MachineChange::Target(target)))
            .on_submit(change(MachineChange::Save)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    ]
    .spacing(6);
    if !machines.config_hosts.is_empty() {
        let mut picks = row![label("Saved", "المحفوظة")]
            .spacing(6)
            .align_y(Alignment::Center);
        for host in &machines.config_hosts {
            picks = picks.push(plan_button(
                host.clone(),
                form.target == *host,
                CostView::Machine(MachineChange::PickConfigHost(host.clone())),
                theme,
            ));
        }
        fields = fields.push(picks);
    }
    fields = fields.push(
        row![
            label("System", "النظام"),
            segmented(
                vec![
                    (
                        "Linux / macOS",
                        form.os == RemoteOs::Unix,
                        change(MachineChange::Os(RemoteOs::Unix)),
                    ),
                    (
                        "Windows",
                        form.os == RemoteOs::Windows,
                        change(MachineChange::Os(RemoteOs::Windows)),
                    ),
                ],
                theme,
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );
    if let Some(problem) = form.problem {
        let message = match problem {
            HostProblem::Name => tr(language, "Give the machine a name.", "اكتب اسمًا للجهاز."),
            HostProblem::NameTaken => {
                tr(language, "That name is already used.", "هذا الاسم مستخدم.")
            }
            HostProblem::Target => tr(
                language,
                "Write a host such as user@203.0.113.5, without spaces.",
                "اكتب عنوانًا مثل user@203.0.113.5 دون مسافات.",
            ),
        };
        fields = fields.push(
            text(message)
                .size(typography::METADATA_SIZE)
                .color(Color::from_rgb8(0xE0, 0x6C, 0x5F)),
        );
    }
    fields = fields.push(muted_line(
        tr(
            language,
            "Signs in with your SSH keys only. Linux and macOS need python3; Windows uses its own PowerShell. A small state file is kept there so later reads are quick.",
            "يدخل بمفاتيح SSH فقط. Linux وmacOS يحتاجان python3، وWindows يستخدم PowerShell الموجود فيه. يُحفظ هناك ملف حالة صغير لتكون القراءات التالية سريعة.",
        ),
        theme,
    ));
    fields = fields.push(
        row![
            Space::new().width(Fill),
            plan_button(
                tr(language, "Cancel", "إلغاء"),
                false,
                CostView::Machine(MachineChange::CloseForm),
                theme,
            ),
            plan_button(
                tr(language, "Add", "إضافة"),
                true,
                CostView::Machine(MachineChange::Save),
                theme,
            ),
        ]
        .spacing(6),
    );
    container(fields)
        .padding([10, 10])
        .width(Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.colors.hover())),
            border: Border {
                radius: 8.0.into(),
                ..Border::default()
            },
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::synced_ago;
    use crate::locale::Language;

    #[test]
    fn sync_times_read_short() {
        let minutes = chrono::TimeDelta::minutes;
        assert_eq!(synced_ago(minutes(0), Language::English), "Synced just now");
        assert_eq!(
            synced_ago(minutes(5), Language::English),
            "Synced 5 min ago"
        );
        assert_eq!(
            synced_ago(minutes(130), Language::English),
            "Synced 2 h ago"
        );
        assert_eq!(synced_ago(minutes(5), Language::Arabic), "قُرئ قبل 5 د");
    }
}
