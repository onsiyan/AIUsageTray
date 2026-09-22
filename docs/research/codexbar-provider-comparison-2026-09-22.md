# مقارنة مزودي الاستخدام مع CodexBar

تاريخ المراجعة: 2026-09-22  
النطاق: نسخة Rust المنسوخة `outputs/CodexUsageMonitor-Rust/rust` فقط، ومقارنة مصدرية مع مشروع CodexBar واختباراته ووثائقه الحالية. نُفذت تحسينات مسار Codex الموضحة أدناه؛ بقية adapters لم تتغير في هذه الدفعة، ولم يُمس مشروع .NET الأصلي. هذه مراجعة لاستعلامات الاستخدام والهوية والحدود وإعادة التعيين والاسترداد من الأخطاء، لا مقارنة واجهات.

## الخلاصة التنفيذية

| المزود | حالة أساس الاستعلام | أعلى فرق ذي أثر |
|---|---|---|
| Codex | جلسة متصفح مستوردة، هوية موثقة، ثم WHAM | استبعاد متعمد لمصادر ملفات/CLI؛ إثراء لوحة الويب الاختياري غير منفذ |
| Claude | تغطية واسعة لـ OAuth وWeb وCLI وAdmin؛ أُصلح عزل OAuth والمؤسسة والتحقق المالي | تدوير/إعادة استيراد Web session بعد انتهاء الجلسة غير منفذ بعد |
| Antigravity | مسارا التطبيق وOAuth؛ الحالات المعطلة/المجهولة لا تتحول إلى صفر زائف | `agy` CLI مؤجل لأن مخرجاته لا تثبت هوية الحساب المراد تحديثه |
| OpenRouter | endpoint الأساسي والمفاتيح المعزولة متوافقان | تعطل `/key` يمنع رصيد `/credits`، وسجل النشاط وBYOK أقل اكتمالًا؛ قبول HTTP مخصص خطر أمني |
| OpenCode Go | Console/API والـmeters الأساسية متوافقة | `endsAt` لا يدخل في إعادة تعيين الشهر، وسلوك التحويلات والـfallback أقل اكتمالًا |

التوصية العامة ليست إعادة كتابة المزودين؛ الأساس موجود. الأولوية هي تثبيت دلالات المصدر والحساب، ثم سدّ حالات الفشل التي قد تنتج بيانات ناقصة أو تنسبها للحساب الخطأ. عولج Codex أولًا مع الحفاظ على قيد الجلسة المستوردة من المتصفح وتعدد الحسابات.

## Codex

**مسارنا المنفذ:** تُستورد Cookies الحساب من ملف متصفح Chromium عبر مسار الإضافة الصريح. قبل WHAM يطلب المحول `/api/auth/session` ويشترط تطابق البريد مع الحساب المحدد؛ عند الاختلاف يتوقف قبل استعلام الاستخدام. لا تُقبل بيانات Bearer/OAuth المنفردة، ولا تُستخدم ملفات Codex أو CLI أو app-server. أي `accessToken` يعيده endpoint الجلسة يُستخدم في الذاكرة لطلب WHAM فقط، مع `ChatGPT-Account-Id` عند توفره.

**المقارنة مع CodexBar:** CodexBar يملك مسارات OAuth وCLI إضافية، ولوحة الويب enrichment اختياري. لم ننسخ مسارات الملفات/CLI لأن سياسة الحسابات هنا تشترط جلسة المتصفح التي اختارها المستخدم وتحقق هويتها؛ هذا فرق مقصود لا فجوة في WHAM أو في نافذتي الاستخدام. لوحة الويب غير منفذة، وهي enrichment منفصل وليست شرطًا لعرض حد الخمس ساعات والأسبوع.

**دقة الحدود:** يقبل parser الآن النسبة الخام `used_percent > 100` عند تجاوز الحصة، بينما تعرض `remaining_percent()` المتبقي بحد أدنى صفر. ويعرض مسار WHAM النوافذ الأساسية والإضافية ورصيد reset credits وتواريخ reset المطلقة.

**اختبارات الانحدار:** تثبت أن session preflight يسبق WHAM، وأن الجلسة المطابقة وحدها تمد الطلب برمزها، وأن mismatch يوقف أي طلب usage، وأن رمزًا بلا Cookies لا يصدر أي استعلام. كما تتيح أداة الاختبار حصر الاستيراد في ملف متصفح بعينه عند تعدد الحسابات.
**مراجع محلية:** [محول OpenAI/WHAM](../../crates/core/src/providers/openai.rs)، [مصادر المصادقة](../../crates/core/src/auth_sources.rs)، [مستورد Cookies](../../crates/windows-auth/src/browser_cookies.rs)، [أداة الاستيراد والاختبار](../../tools/codex-probe/src/main.rs).
**مرجع CodexBar:** [Codex provider](https://github.com/steipete/CodexBar/blob/main/docs/codex.md)، [دليل OAuth](https://github.com/steipete/CodexBar/blob/main/docs/codex-oauth.md)، [Codex UsageFetcher](https://raw.githubusercontent.com/steipete/CodexBar/main/Sources/CodexBarCore/UsageFetcher.swift).

## Claude

**المتوافق:** توجد محولات OAuth `/api/oauth/usage`، وWeb session عبر `/api/organizations/{id}/usage`، وCLI، وAdmin API؛ مع نوافذ الجلسة والأسبوع وحدود النماذج، وتواريخ reset، وبيانات extra usage/credits. ترتيب Auto الأساسي للتطبيق OAuth ثم CLI ثم Web يطابق التوثيق الحالي للمرجع.

**الفروق التي عولجت:**

1. اختيار OAuth الصريح صار نهائيًا ولا ينتقل إلى CLI، منعًا لعرض استعمال مصدر/حساب مختلف. الرجوع بين المصادر بقي لسلوك Auto فقط.
2. اختيار المنظمة يفهم `capabilities` الحديثة مع الاحتفاظ بدعم الأعلام القديمة. وعند وجود organization ID مربوط بالحساب لا يجري اختيار مؤسسة بديلة إذا غابت المؤسسة المطلوبة.
3. عقد spend/credits يحمل `currency_code` اختياريًا من دون تغيير JSON للسجلات القديمة عند غياب العملة. Extra usage يرفض القيم السالبة وغير المنتهية والحد غير الموجب؛ Overage يتطلب `is_enabled: true` وعملة، وPrepaid يتطلب مبلغًا وعملة صالحين. الدمج لا يخلط مبالغ بعملتين مختلفتين.
4. Cloudflare challenge صار خطأً مستقلًا عن 401: يحتفظ المنسق بآخر قياس ويعرضه كقديم بدل إبطال الحساب. 401 يبقى رفض اعتماد حقيقيًا.

**الفرق المتبقي:** لا يوجد بعد تدوير دائم لـ `sessionKey` من رد `Set-Cookie` أو إعادة استيراده بعد 401. الحساب يحفظ `browser_profile_id` من دون نوع المتصفح، ومحول Claude الحالي لا يملك مسار كتابة إلى مخزن الاعتمادات؛ لذلك لا يمكن إعادة الاستيراد بدقة وأمان لحساب محدد دون توسيع عقد ملف المتصفح/التخزين والتحقق من البريد والمؤسسة قبل حفظ الجلسة الجديدة.

الماسح المحلي لسجل JSONL ولوحة Admin التفصيلية اليومية في CodexBar توسعات history/spend وليست شرطًا لصحة polling للحصص، لذا تُؤجل إذا بقي النطاق الحالي مراقبة quota.

**مراجع محلية:** [Claude adapter](../../crates/core/src/providers/claude.rs)، [Claude source planner](../../crates/core/src/providers/claude_planner.rs)، [مصادر المصادقة](../../crates/core/src/auth_sources.rs).  
**مرجع CodexBar:** [Claude provider](https://github.com/steipete/CodexBar/blob/main/docs/claude.md)، [Claude Web fetcher](https://github.com/steipete/CodexBar/blob/main/Sources/CodexBarCore/Providers/Claude/ClaudeWeb/ClaudeWebAPIFetcher.swift)، [Claude OAuth fetcher](https://github.com/steipete/CodexBar/blob/main/Sources/CodexBarCore/Providers/Claude/ClaudeOAuth/ClaudeOAuthUsageFetcher.swift).

## Antigravity

**المتوافق:** محليًا توجد استدعاءات RPC البعيدة نفسها تقريبًا: `loadCodeAssist` و`onboardUser` و`fetchAvailableModels` و`retrieveUserQuota` و`retrieveUserQuotaSummary`، إضافة إلى probing لخدمة `language_server` المحلية. تحليل summary يدعم العائلتين والنوافذ الخماسية/الأسبوعية ومعلومات reset، ويتعامل الآن مع buckets المعطلة أو المجهولة كبيانات غير متاحة مع الاحتفاظ بسياق reset، بدل اصطناع استعمال 0%.

**الفروق المؤثرة:**

1. لا يوجد `agy` CLI source. CodexBar يجرّبه بعد تطبيق Antigravity المحلي وقبل IDE/OAuth، ويقرأ summary أغنى عند إغلاق التطبيق. لكن المصدر المرجعي نفسه يذكر أن إخراج `agy` لا يقدم هوية حساب؛ لذلك أُجّل إسناده إلى حساباتنا المتعددة إلى أن يوجد ربط موثق وآمن، بدل نسب quota لحساب نشط خطأً.
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

1. **Antigravity:** تقوية تصنيف process ownership وجاهزية `language_server` ومطابقة الحساب؛ لا يُضاف `agy` قبل وجود دليل account identity قابل للربط.
2. **Claude:** مسار تدوير/إعادة استيراد cookie مربوط بمتصفح وملف وحساب صراحةً، مع تحقق email وorganization قبل حفظ الجلسة؛ بقية الفروق الأربع السابقة عولجت.
3. **OpenRouter:** افصل فشل key عن credits، ثم أصلح activity آخر يوم/التحقق/dedup وBYOK، وارفض HTTP.
4. **OpenCode Go:** ابدأ بـ `endsAt` وحقول reset، ثم redirect HTTPS المحدود وفصل الرؤوس، وبعدها مصفوفة fallback/balance.

هذه البنود تخص adapters التي لم تُحدّث بعد. قبل كل تغيير، تُراجع نسخة CodexBar من المصدر والاختبارات المقابلة لأن فرع `main` متغير، ثم تُكيّف الفكرة مع Windows وعزل الحسابات بدل نسخ تنفيذ macOS حرفيًا.

## التحقق وحدود المراجعة

- أحدث تحقق للمستودع: `cargo test --workspace` نجح، بما فيه 89 اختبارًا وحدويًا و11 اختبار عقد لـ core، و12 اختبارًا لـ windows-auth، و3 اختبارات لـ Codex probe؛ كما نجح `cargo fmt --all -- --check`.
- بقية المزودين: مقارنة source/docs/tests فقط في هذه الجولة؛ لم يُشغّل تكامل حقيقي مع حسابات أو خدمات خارجية، ولم تُشغّل اختبارات Swift/CodexBar.
- CodexBar source snapshot reported by the OpenRouter audit: `173afc88a4171f013c53f8c5167f64808565f913`; the upstream `main` can change after this review date.
- لا تحتوي هذه المراجعة على بيانات اعتماد أو مفاتيح حسابات.
