# مقارنة مزودي الاستخدام مع CodexBar

تاريخ المراجعة: 2026-09-22  
النطاق: نسخة Rust المنسوخة `outputs/CodexUsageMonitor-Rust/rust` فقط، ومقارنة مصدرية مع مشروع CodexBar واختباراته ووثائقه الحالية. لم يتغير أي adapter أو كود تشغيل، ولم يُمس مشروع .NET الأصلي. هذه مراجعة لاستعلامات الاستخدام والهوية والحدود وإعادة التعيين والاسترداد من الأخطاء، لا مقارنة واجهات.

## الخلاصة التنفيذية

| المزود | حالة أساس الاستعلام | أعلى فرق ذي أثر |
|---|---|---|
| Codex | قوي في WHAM والاستخدام الأساسي | حدود parser/RPC ومصدر app-server؛ لوحة الويب إضافات اختيارية وليست فجوة في حد الاستخدام الأساسي |
| Claude | تغطية واسعة لـ OAuth وWeb وCLI وAdmin | اختيار OAuth الصريح قد يسقط إلى CLI، وفك organizations وبيانات الجلسة أقل متانة |
| Antigravity | مسار التطبيق وOAuth موجودان | لا يوجد مصدر `agy` CLI، وهو مسار CodexBar الأهم عند إغلاق التطبيق لإبقاء معلومات الحصص أغنى |
| OpenRouter | endpoint الأساسي والمفاتيح المعزولة متوافقان | تعطل `/key` يمنع رصيد `/credits`، وسجل النشاط وBYOK أقل اكتمالًا؛ قبول HTTP مخصص خطر أمني |
| OpenCode Go | Console/API والـmeters الأساسية متوافقة | `endsAt` لا يدخل في إعادة تعيين الشهر، وسلوك التحويلات والـfallback أقل اكتمالًا |

التوصية العامة ليست إعادة كتابة المزودين؛ الأساس موجود. الأولوية هي تثبيت دلالات المصدر والحساب، ثم سدّ حالات الفشل التي قد تنتج بيانات ناقصة أو تنسبها للحساب الخطأ. وبما أن Codex هو الأهم، يبدأ التنفيذ اللاحق منه مع احترام قيد تعدد الحسابات أدناه.

## Codex

**المتوافق:** محول WHAM المحلي يطلب `wham/usage` ورصيد إعادة التعيين، مع سياق Bearer/Cookie ومعرّف الحساب والرؤوس اللازمة. كما يوجد fallback عبر Codex app-server، ومحلل لنوافذ الحد والائتمانات والنوافذ الإضافية. هذه هي عائلة المصادر الأساسية التي توثقها CodexBar.

**الفروق المؤثرة:**

1. محليًا، Auto يرتب المصادر PAT ثم OAuth ثم CLI؛ CodexBar يرتب OAuth ثم CLI، ويجعل لوحة الويب enrichment منفصلًا اختياريًا. المحول المحلي لا ينفذ Web Dashboard أصلًا. لوحة الويب في المرجع اختيارية وتضيف أمورًا مثل سجل الائتمانات وتجديد الاشتراك وتفصيل الاستخدام؛ لا ينبغي خلط غيابها مع غياب WHAM أو حدّ الخمس ساعات/الأسبوع.
2. محليا، parser يرفض `usedPercent > 100`. CodexBar يحتفظ بنسبة الاستهلاك الخام عندما تتجاوز الحصة، ثم يقيّد العرض فقط. الرفض قد يحول حصة مستهلكة بالكامل أو متجاوزة إلى خطأ parsing بدل عرضها.
3. أخطاء RPC المحلية تتحول إلى خطأ عام ولا تستخرج usage/credits القابلة للاسترداد من جسم خطأ RPC. كما أن `requiresOpenaiAuth` وحالة `account: null` ليستا ممثلتين كحالة تسجيل دخول مميزة.
4. اختبارات المصدر المحلي لا تثبت بعد عقود النقل والرؤوس، واسترداد JSON من أخطاء RPC، وقيم over-quota، وحالة `requiresOpenaiAuth`.

**قيد الحسابات:** توجد في النسخة الحالية قراءة بيانات Codex المحلية واستخدام app-server كمسار usage. هذا يشبه CodexBar، لكنه لا يثبت وحده عزلًا صحيحًا لحسابات المستخدم إذا لم تكن كل قراءة/عملية مربوطة بملف الحساب المقصود. وبالنظر إلى شرطك السابق لتعدد حسابات Codex وعدم الاعتماد على مصدر CLI/ملفات المصادقة كمرجع الاستخدام، يجب حسم هذا الحد قبل اعتبار fallback المحلي مكافئًا مقبولًا؛ لا نوسّعه تلقائيًا لمجرد أن المرجع يفعله.

**مراجع محلية:** [محول OpenAI/WHAM](../../crates/core/src/providers/openai.rs)، [مخطط المصادر](../../crates/core/src/providers/codex_planner.rs)، [Codex app-server](../../crates/core/src/providers/codex_cli.rs).  
**مرجع CodexBar:** [Codex provider](https://github.com/steipete/CodexBar/blob/main/docs/codex.md)، [دليل OAuth](https://github.com/steipete/CodexBar/blob/main/docs/codex-oauth.md)، [Codex UsageFetcher](https://raw.githubusercontent.com/steipete/CodexBar/main/Sources/CodexBarCore/UsageFetcher.swift).

## Claude

**المتوافق:** توجد محولات OAuth `/api/oauth/usage`، وWeb session عبر `/api/organizations/{id}/usage`، وCLI، وAdmin API؛ مع نوافذ الجلسة والأسبوع وحدود النماذج، وتواريخ reset، وبيانات extra usage/credits. ترتيب Auto الأساسي للتطبيق OAuth ثم CLI ثم Web يطابق التوثيق الحالي للمرجع.

**الفروق المؤثرة:**

1. عند اختيار OAuth صراحة، مخطط Rust يضيف CLI بعد OAuth؛ حلقة التنفيذ تنتقل للمصدر التالي على أخطاء OAuth العادية. CodexBar يذكر أن المصدر الصريح يتجاوز fallback التلقائي. قد يؤدي ذلك إلى عرض استعمال CLI مختلف عن جلسة OAuth التي اختارها المستخدم، وهو خطر تعدد حسابات لا مجرد فرق ترتيب.
2. اختيار منظمة Web محليًا يعتمد حقولًا boolean أقدم مثل `has_chat_capability` و`is_api_only`. CodexBar الحالي يقرأ أيضًا مصفوفة `capabilities`؛ غياب هذا الشكل قد يختار منظمة API-only أو لا يجد المنظمة الصحيحة عند تعددها.
3. فحص spend/credits المحلي أوسع تساهلًا من المرجع في حقول `enabled` والقيم غير المنتهية/السالبة، ونموذج الاستخدام الموحد لا يحمل عملة صريحة. يجب عدم خلط أرصدة بعملات مختلفة أو إظهار قيم malformed على أنها صالحة.
4. جلسة Web المحلية تُستورد وتُربط بالحساب، لكن لا يوجد مسار واضح لإعادة استيراد الجلسة/تدويرها تلقائيًا بعد رفضها أو تحديث cookie. المرجع يملك إدارة واستردادًا لـ cookies مع الاحتفاظ بالقياس السابق عند تحديات الشبكة. يلزم فصل 401 (جلسة غير صالحة) عن Cloudflare/تعذر الشبكة قبل إعادة الاستيراد.

الماسح المحلي لسجل JSONL ولوحة Admin التفصيلية اليومية في CodexBar توسعات history/spend وليست شرطًا لصحة polling للحصص، لذا تُؤجل إذا بقي النطاق الحالي مراقبة quota.

**مراجع محلية:** [Claude adapter](../../crates/core/src/providers/claude.rs)، [Claude source planner](../../crates/core/src/providers/claude_planner.rs)، [مصادر المصادقة](../../crates/core/src/auth_sources.rs).  
**مرجع CodexBar:** [Claude provider](https://github.com/steipete/CodexBar/blob/main/docs/claude.md)، [Claude Web fetcher](https://github.com/steipete/CodexBar/blob/main/Sources/CodexBarCore/Providers/Claude/ClaudeWeb/ClaudeWebAPIFetcher.swift)، [Claude OAuth fetcher](https://github.com/steipete/CodexBar/blob/main/Sources/CodexBarCore/Providers/Claude/ClaudeOAuth/ClaudeOAuthUsageFetcher.swift).

## Antigravity

**المتوافق:** محليًا توجد استدعاءات RPC البعيدة نفسها تقريبًا: `loadCodeAssist` و`onboardUser` و`fetchAvailableModels` و`retrieveUserQuota` و`retrieveUserQuotaSummary`، إضافة إلى probing لخدمة `language_server` المحلية. تحليل summary يدعم العائلتين والنوافذ الخماسية/الأسبوعية ومعلومات reset، مع اختبار للتحقق من بيانات quotas التي تبدو كلها 100%.

**الفروق المؤثرة:**

1. لا يوجد `agy` CLI source. CodexBar يجرّبه بعد تطبيق Antigravity المحلي وقبل IDE/OAuth، ويستطيع إعادة استخدام خدمته المحلية أو تشغيل جلسة مضبوطة، ثم يقرأ summary الأغنى حتى مع إغلاق التطبيق. المحلي يعتمد اكتشاف `language_server.exe` المحدد ثم OAuth؛ لذا يفقد هذا المسار عندما لا يكون مصدر التطبيق حاضرًا. تفاصيل تشغيل `agy` في المرجع مكتوبة أساسًا لمنصات أخرى ولا يجوز نسخ آلية PTY/إدارة العمليات إلى Windows بلا تكييف.
2. اكتشاف Windows المحلي يطابق اسم process واحدًا، يجمع منافذ الاستماع ثم يجرب endpoints؛ لا يصنف app/IDE/CLI ولا يطبق جاهزية قائمة على نجاح quota endpoint أو استراتيجية قوية لاختيار عدة endpoints. هذا يزيد احتمال الفشل المؤقت أو اختيار خدمة أخرى عند تعدد العمليات.
3. فحص local يتطلب تطابق email من `GetUserStatus` قبل قبول summary؛ المرجع قد يحتفظ بحصة صحيحة عندما endpoint الهوية مفقود. بالمقابل، مسار OAuth المحلي يوسم البريد المحفوظ باعتباره هوية verified حتى لو لم يثبت رد الاستخدام ذلك؛ ينبغي توضيح مصدر الثقة وعدم عرض هوية مستنتجة كأنها مؤكدة.
4. تمثيل OAuth/model quotas أقل اكتمالًا من دلالات summary المحلي: CodexBar يذكر أن OAuth قد يعيد model buckets بدل مجموعتي Antigravity الظاهرتين في التطبيق؛ بعض مسارات fallback المحلية تختار صفًا أوليًا/أعلى استخدام دون تصنيف عائلات مكافئ كامل، ما قد يجعل الصف الأساسي غير ممثل للحصة الأشد.

**مراجع محلية:** [Antigravity adapter](../../crates/core/src/providers/antigravity.rs)، [مصادر المصادقة](../../crates/core/src/auth_sources.rs).  
**مرجع CodexBar:** [Antigravity provider](https://github.com/steipete/CodexBar/blob/main/docs/antigravity.md)، [local status probe](https://raw.githubusercontent.com/steipete/CodexBar/main/Sources/CodexBarCore/Providers/Antigravity/AntigravityStatusProbe.swift)، [OAuth quota parser](https://raw.githubusercontent.com/steipete/CodexBar/main/Sources/CodexBarCore/Providers/Antigravity/AntigravityQuotaSummaryParser.swift).

## OpenRouter

**المتوافق:** `/key` و`/credits` و`/activity` موجودة. المفتاح الأساسي مخصص للـquota/credits، ومفتاح الإدارة منفصل للنشاط؛ كلاهما مربوط بسجل الحساب. Parsing الحد والـremaining وreset والفترات المجدولة قريب من plugin المرجعي، مع اختبارات محلية جيدة للمسار السليم.

**الفروق المؤثرة:**

1. الاستعلام المحلي عن `/key` يسبق `/credits`، وفشل أو JSON غير صالح في الأول ينهي adapter؛ في CodexBar كل endpoint degradable بمفرده، فيبقى الرصيد متاحًا إن تعطل endpoint الحد. هذا أهم فرق في تحمل الأعطال.
2. CodexBar يجمع نشاط آخر 30 يوم UTC بطلب التاريخ المكتمل الأخير مع history، ثم يتحقق من الصفوف ويزيل المكرر. المحلي يكتفي بطلب activity واحد وتحقق أضعف؛ كما أن `byok_usage_inference` يبقى metadata ولا يدخل تقدير الإنفاق مثل المرجع. النتيجة سجل/إنفاق قد يكون ناقصًا أو صفوفًا مدموجة.
3. عنوان API مخصص عبر HTTP مقبول محليًا؛ CodexBar يشترط HTTPS. إذا مر مفتاح API لعنوان HTTP مخصص فقد يُرسل دون تشفير. يجب أن يرفض المحول HTTP بدل محاولة مسايرة ذلك.
4. عند `limit_remaining` فوق الحد، المرجع يقصّه إلى المجال ويعرض 0% استخدام، أما المحلي قد يسقط إلى fallback آخر. يلزم strict typing/حدود للأرقام واختبارات للحالات السالبة والزائدة، مع المحافظة على إضافات local مثل free-model metrics إن كانت نافعة.

**مراجع محلية:** [OpenRouter adapter](../../crates/core/src/providers/openrouter.rs)، [مصادر المفاتيح](../../crates/core/src/auth_sources.rs)، [عقود adapters](../../crates/core/tests/contracts.rs).  
**مرجع CodexBar:** [OpenRouter provider notes](https://github.com/steipete/CodexBar/blob/main/docs/openrouter.md)، [OpenRouter plugin implementation](https://github.com/steipete/CodexBar/blob/main/Sources/CodexBarCore/Resources/Plugins/openrouter.js)، [OpenRouter activity tests](https://github.com/steipete/CodexBar/blob/main/Tests/CodexBarTests/OpenRouterUsageStatsTests.swift).

## OpenCode Go

**المتوافق:** مسارات Console (`zen/go/v1/usage` و`console/api/...`) والـworkspace header، وmeters للـ5h/week/month، وتحويل micro-cent وfallbackات billing الأساسية ممثلة محليًا. استراتيجية مصدر account-scoped تتبع web/local/API، وSQLite المحلي معلّم كتقدير لا كقياس authoritative.

**الفروق المؤثرة:**

1. Parser Console المحلي لا يمرر `access.endsAt` كموعد تجديد ولا يملأ reset الشهر منه، بينما CodexBar يستخدمه لـ `renewsAt` وmonth reset حين `month.resetsAt` غائب. كما أن بعض أسماء حقول reset المتداولة مثل `reset_sec` غير مقبولة محليًا.
2. transport المحلي يعطل redirects كليًا، بينما المرجع يسمح بتحويلات HTTPS على النطاق نفسه. هذا قد يكسر redirect مشروعًا؛ العلاج ليس اتباع أي redirect، بل سياسة provider-specific تقيد HTTPS والنطاق.
3. fallback المحلي تسلسلي وأقل غنيًا من المرجع: لا يوازي subscription مع Zen balance الاختياري ولا يحتفظ بنفس قواعد fallback عند `noSubscription` مقابل أخطاء الشبكة/التحليل. وهذا قد يرفع زمن التحديث أو يفقد balance صالحًا عند تعطل مسار آخر.
4. بعض الحقول المباشرة مثل `usage` و`usage.renewAt` لا تُقرأ في كل أشكال API مثل المرجع، وتطبيع workspace override المحلي أقل صرامة. كذلك ينبغي فصل رؤوس API Bearer عن Cookie headers لتجنب إرسال cookie إلى API لا يحتاجها.

**مراجع محلية:** [OpenCode Go adapter](../../crates/core/src/providers/opencode_go.rs)، [مصدر usage المحلي](../../crates/core/src/providers/opencode_go_local.rs)، [HTTP transport](../../crates/core/src/transport.rs).  
**مرجع CodexBar:** [OpenCode provider notes](https://github.com/steipete/CodexBar/blob/main/docs/opencode.md)، [OpenCode Go fetcher](https://raw.githubusercontent.com/steipete/CodexBar/main/Sources/CodexBarCore/Providers/OpenCodeGo/OpenCodeGoUsageFetcher.swift)، [legacy fallback](https://raw.githubusercontent.com/steipete/CodexBar/main/Sources/CodexBarCore/Providers/OpenCodeGo/OpenCodeGoLegacyFallback.swift).

## المنسق المشترك

في Rust المحلي، Adaptive الافتراضي هو 2 دقيقة بعد تفاعل حديث، 5 دقائق حتى ساعة، 15 دقيقة حتى 4 ساعات، ثم 30 دقيقة؛ ويهبط إلى 30 دقيقة في low-power/thermal constrained. نشاط البرمجة الحديث يسرّع الخمول الطويل إلى 5 دقائق. هناك أيضًا refresh قريب من reset مع grace قدره 30 ثانية وحد أدنى 5 ثوانٍ. راجع [RefreshCoordinator](../../crates/core/src/refresh.rs).

CodexBar يوثق نطاقًا تكيفيًا مماثلًا 2–30 دقيقة، لكنه يجعل قراءة نشاط الوكلاء وضعًا منفصلًا اختياريًا يتطلب الموافقة، بينما السياسة المحلية تستخدم إشارة coding activity في Adaptive نفسه. إذًا الفارق ليس مدد polling وحدها؛ بل مصدر إشارة النشاط وحدود موافقة المستخدم. يجب إبقاء هذا فرقًا للمنسق، لا نسبته إلى adapters.

مرجع: [CodexBar refresh loop](https://github.com/steipete/CodexBar/blob/main/docs/refresh-loop.md).

## ترتيب العمل المقترح

1. **Codex أولًا:** حسم حدّ مصادر المصادقة لكل حساب، ثم إصلاح قبول over-quota وحالات RPC/auth، وإضافة tests لعقود الخطأ. لا تبدأ بـ Web Dashboard؛ هي enrichment اختيارية.
2. **Antigravity:** تصميم مصدر `agy` مناسب لـ Windows مع readiness/account identity/ownership آمنة، ثم تقوية اكتشاف `language_server` ومطابقة الحساب.
3. **Claude:** اجعل اختيار OAuth الصريح نهائيًا، حدّث parsing للـorganizations، ثم عالج cookie rotation وتصنيف Cloudflare/401 وقواعد صلاحية spend/credits.
4. **OpenRouter:** افصل فشل key عن credits، ثم أصلح activity آخر يوم/التحقق/dedup وBYOK، وارفض HTTP.
5. **OpenCode Go:** ابدأ بـ `endsAt` وحقول reset، ثم redirect HTTPS المحدود وفصل الرؤوس، وبعدها مصفوفة fallback/balance.

هذه الترتيبات نتائج المقارنة وليست تغييرات منفذة. قبل كل تغيير، تُراجع نسخة CodexBar من المصدر والاختبارات المقابلة مرة أخرى لأن فرع `main` متغير، ثم تُكيّف الفكرة مع Windows وعزل الحسابات بدل نسخ تنفيذ macOS حرفيًا.

## التحقق وحدود المراجعة

- شغّل المراجعون محليًا: `cargo test -p codex-usage-core claude -- --nocapture` — 24 اختبارًا ناجحًا؛ و`cargo test -p codex-usage-core openrouter` — 9 اختبارات ناجحة.
- بقية المزودين: مقارنة source/docs/tests فقط في هذه الجولة؛ لم يُشغّل تكامل حقيقي مع حسابات أو خدمات خارجية، ولم تُشغّل اختبارات Swift/CodexBar.
- CodexBar source snapshot reported by the OpenRouter audit: `173afc88a4171f013c53f8c5167f64808565f913`; the upstream `main` can change after this review date.
- لا تحتوي هذه المراجعة على بيانات اعتماد أو مفاتيح حسابات.
