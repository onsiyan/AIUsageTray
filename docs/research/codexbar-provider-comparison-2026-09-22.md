# مقارنة مزودي الاستخدام مع CodexBar

تاريخ المراجعة: 2026-09-23
النطاق: نسخة Rust المنسوخة `outputs/CodexUsageMonitor-Rust/rust` فقط، ومقارنة مصدرية مع مشروع CodexBar واختباراته ووثائقه الحالية. نُفذت تحسينات محددة في مسارات Codex وClaude وAntigravity وOpenRouter وOpenCode Go؛ الفروق المتبقية مذكورة لكل مزود، ولم يُمس مشروع .NET الأصلي. هذه مراجعة لاستعلامات الاستخدام والهوية والحدود وإعادة التعيين والاسترداد من الأخطاء، وليست اختبارًا حيًا للخدمات أو مقارنة واجهات.

## الخلاصة التنفيذية

| المزود | حالة أساس الاستعلام | أعلى فرق ذي أثر |
|---|---|---|
| Codex | جلسة متصفح مستوردة، هوية موثقة، ثم WHAM | استبعاد متعمد لمصادر ملفات/CLI؛ إثراء لوحة الويب الاختياري غير منفذ |
| Claude | تغطية OAuth وWeb وCLI وAdmin؛ تدوير Web session، واستعادة واحدة مربوطة بالحساب بعد 401 | سجلات الحساب القديمة التي لا تملك browser kind تحتاج ربطًا جديدًا؛ لم يُختبر مع خدمة حية |
| Antigravity | مسارا التطبيق وOAuth؛ الملخص والعناصر الإضافية تتبعان تقسيم العائلات وكبح التكرار في CodexBar | إسناد آمن لمصدر `agy` مؤجل لغياب دليل هوية الحساب |
| OpenRouter | `/key` و`/credits` وActivity مستقلة، مع HTTPS وسجل 30 يومًا مكتملًا وBYOK | لم يُختبر مع خدمة OpenRouter حية؛ الأدلة الحالية اختبارات اصطناعية ومقارنة مصدرية |
| OpenCode Go | Console/API والـmeters و`endsAt` وحقول reset، مع redirects محدودة لنفس الأصل | إثراء الرصيد والـfallback ما زالا تسلسليين وأقل اكتمالًا من المرجع؛ لا يوجد اختبار حي |

التوصية العامة ليست إعادة كتابة المزودين؛ الأساس موجود. الأولوية هي تثبيت دلالات المصدر والحساب، ثم سدّ حالات الفشل التي قد تنتج بيانات ناقصة أو تنسبها للحساب الخطأ. عولج Codex أولًا مع الحفاظ على قيد الجلسة المستوردة من المتصفح وتعدد الحسابات.

## Codex

**مسارنا المنفذ:** تُستورد Cookies الحساب من ملف متصفح Chromium عبر مسار الإضافة الصريح. قبل WHAM يطلب المحول `/api/auth/session` ويشترط تطابق البريد مع الحساب المحدد؛ عند الاختلاف يتوقف قبل استعلام الاستخدام. لا تُقبل بيانات Bearer/OAuth المنفردة، ولا تُستخدم ملفات Codex أو CLI أو app-server. أي `accessToken` يعيده endpoint الجلسة يُستخدم في الذاكرة لطلب WHAM فقط، مع `ChatGPT-Account-Id` عند توفره.

**المقارنة مع CodexBar:** CodexBar يملك مسارات OAuth وCLI إضافية، ولوحة الويب enrichment اختياري. لم ننسخ مسارات الملفات/CLI لأن سياسة الحسابات هنا تشترط جلسة المتصفح التي اختارها المستخدم وتحقق هويتها؛ هذا فرق مقصود لا فجوة في WHAM أو في نافذتي الاستخدام. لوحة الويب غير منفذة، وهي enrichment منفصل وليست شرطًا لعرض حد الخمس ساعات والأسبوع.

**دقة الحدود:** يقبل parser الآن النسبة الخام `used_percent > 100` عند تجاوز الحصة، بينما تعرض `remaining_percent()` المتبقي بحد أدنى صفر. ويعرض مسار WHAM النوافذ الأساسية والإضافية ورصيد reset credits وتواريخ reset المطلقة.

**فشل الإثراءات الاختيارية:** إذا فشلت استعلامات reset-credit inventory أو صرف مساحة العمل أو رصيدها، تبقى لقطة WHAM الأساسية صالحة ويضيف المحول تشخيصًا آمنًا لكل مصدر يوضح فئة HTTP/النقل/التحليل، مع `Retry-After` حين يرسله الخادم. لا تُنسخ أجسام الردود أو بيانات الطلب الحساسة إلى التشخيص. هذا يجعل نقص الإثراء ظاهرًا بدل إسقاطه بصمت أو تحويله إلى فشل للحساب.

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
5. ردود Claude Web الناجحة التي تحمل `Set-Cookie: sessionKey=...` تُستخدم لتحديث الجلسة لبقية استعلام الحساب، ثم تُحفظ شرطيًا في مخزن الاعتمادات الآمن للحساب نفسه بعد تطابق البريد. حفظ القيمة لا يستبدل جلسة أحدث، ويحافظ على بقية Cookies وOAuth material؛ وعند غياب إثبات الهوية لا تُحفظ القيمة ويظهر تشخيص. اختبارات الحسابين المتزامنين تثبت عدم تبادل Cookies بينهما، بما في ذلك إثراء OAuth ببيانات Web.
6. بعد HTTP 401 فقط، يعيد مسار Windows قراءة Cookie من زوج المتصفح/الملف المحفوظين للحساب؛ لا يفتح متصفحًا ولا يبحث في ملفات أخرى. يجلب الهوية والمؤسسة المقصودة قبل حفظ الجلسة الجديدة، ويتابع أي `Set-Cookie` أثناء هذا الفحص، ثم يجرب استعلام الاستخدام مرة واحدة. 403/Cloudflare لا يشغّل الاستعادة، وهوية غير مطابقة لا تُحفظ ولا يُرسل بعدها طلب استخدام. اختبارات Windows/core تثبت اختيار الملف المحدد، تدوير الجلسة أثناء فحص الهوية، منع التداخل بين الحسابات، ومحاولة واحدة عند استمرار 401.

**الحدود المتبقية:** سجلات SQLite السابقة تُرحّل مع `browser_kind = NULL`، ولا تُستنتج لها هوية المتصفح بالتخمين؛ يلزم إعادة ربطها عبر مسار الإضافة كي تصبح مؤهلة للاستعادة. إذا تعذر فتح قاعدة Cookies للملف المحفوظ أو فشل فحص الهوية، يبقى رفض 401 الأصلي وتظل الجلسة المخزنة كما هي. لم يُجر اختبار بحساب Claude حي في هذه الدفعة.

الماسح المحلي لسجل JSONL ولوحة Admin التفصيلية اليومية في CodexBar توسعات history/spend وليست شرطًا لصحة polling للحصص، لذا تُؤجل إذا بقي النطاق الحالي مراقبة quota.

**مراجع محلية:** [Claude adapter](../../crates/core/src/providers/claude.rs)، [Claude source planner](../../crates/core/src/providers/claude_planner.rs)، [مصادر المصادقة](../../crates/core/src/auth_sources.rs).  
**مرجع CodexBar:** [Claude provider](https://github.com/steipete/CodexBar/blob/main/docs/claude.md)، [Claude Web fetcher](https://github.com/steipete/CodexBar/blob/main/Sources/CodexBarCore/Providers/Claude/ClaudeWeb/ClaudeWebAPIFetcher.swift)، [Claude OAuth fetcher](https://github.com/steipete/CodexBar/blob/main/Sources/CodexBarCore/Providers/Claude/ClaudeOAuth/ClaudeOAuthUsageFetcher.swift).

## Antigravity

**المتوافق:** محليًا توجد استدعاءات RPC البعيدة نفسها تقريبًا: `loadCodeAssist` و`onboardUser` و`fetchAvailableModels` و`retrieveUserQuota` و`retrieveUserQuotaSummary`، إضافة إلى probing لخدمة `language_server` المحلية. تحليل summary يدعم العائلتين والنوافذ الخماسية/الأسبوعية ومعلومات reset، ويتعامل الآن مع buckets المعطلة أو المجهولة كبيانات غير متاحة مع الاحتفاظ بسياق reset، بدل اصطناع استعمال 0%.

**الفروق المؤثرة:**

1. لا يوجد `agy` CLI source. CodexBar يجرّبه بعد تطبيق Antigravity المحلي وقبل IDE/OAuth، ويقرأ summary أغنى عند إغلاق التطبيق. لكن المصدر المرجعي نفسه يذكر أن إخراج `agy` لا يقدم هوية حساب؛ لذلك أُجّل إسناده إلى حساباتنا المتعددة إلى أن يوجد ربط موثق وآمن، بدل نسب quota لحساب نشط خطأً.
2. تم تضييق اكتشاف Windows المحلي ليشترط مسارًا يحتوي مقطع Antigravity صريحًا، ثم يجمع منافذ الاستماع المملوكة للعملية؛ وبذلك لا يكفي `--app_data_dir` عام لضم عملية برنامج آخر. كما يقارن الآن snapshots المطابقة للحساب عبر endpoints وفق معيار اكتمال CodexBar (أفضلية summary، وعدد المجموعات والحصص المعروفة، والهوية والخطة)، ويجلب `GetUserStatus` مرة واحدة لكل endpoint. ما يزال لا يصنف app/IDE/CLI أو يميز منافذ خادم الإضافة كما يفعل المرجع؛ التحقق من الجاهزية يتم حاليًا بطلبات RPC نفسها، لا بمرحلة resolution مستقلة قبل جمع snapshot.
3. عولجت نسبة الهوية في مسار OAuth البعيد: ردود quota لا تحمل Google identity، لذلك لا تُعاد كـ`VerifiedIdentity` ولا يُملأ `observed_email` من سجل الحساب. يبقى `project_id` في `snapshot.response_account_id` بدل أن يستبدل Google `provider_account_id`، وتبقى هوية الحساب التي جُمعت عند ربط OAuth منفصلة عن سياق مشروع الاستخدام. فحص local ما زال يشترط تطابق email من `GetUserStatus` قبل قبول summary؛ المرجع قد يحتفظ بحصة صحيحة عندما endpoint الهوية مفقود.
4. في مسارات model quotas أصبح `primary` ممثل Gemini الأكثر تقييدًا و`secondary` ممثل Claude/GPT الأكثر تقييدًا؛ صفوف lite/autocomplete/image لا تقود الملخص، وتبقى metrics الخام محفوظة. fallback مجهول العائلة محصور في ردود التطبيق المحلية. صف OAuth الإضافي يُكبح الآن فقط حين يطابق ممثل pool في قيمة quota وموعد reset معروف متطابق؛ الصفوف المحلية أو ذات reset المفقود/المختلف تبقى مستقلة كما في المرجع. كما تُطبع معرّفات Gemini Flash القديمة التسعة إلى `gemini-3.7-flash` وتُدمج الصفوف ذات المعرّف canonical نفسه باختيار الاستخدام المعروف ثم الأكثر تقييدًا، مع إبقاء metrics الخام لكل الصفوف ومعرّفاتها الأصلية.

**مراجع محلية:** [Antigravity adapter](../../crates/core/src/providers/antigravity.rs)، [مصادر المصادقة](../../crates/core/src/auth_sources.rs).  
**مرجع CodexBar:** [Antigravity provider](https://github.com/steipete/CodexBar/blob/main/docs/antigravity.md)، [local status probe](https://raw.githubusercontent.com/steipete/CodexBar/main/Sources/CodexBarCore/Providers/Antigravity/AntigravityStatusProbe.swift)، [OAuth quota parser](https://raw.githubusercontent.com/steipete/CodexBar/main/Sources/CodexBarCore/Providers/Antigravity/AntigravityQuotaSummaryParser.swift)، [تغيير كبح صفوف الـpool المكررة](https://github.com/steipete/CodexBar/pull/3583).

## OpenRouter

**المتوافق:** `/key` و`/credits` و`/activity` موجودة. المفتاح الأساسي مخصص للـquota/credits، ومفتاح الإدارة منفصل للنشاط؛ كلاهما مربوط بسجل الحساب. Parsing الحد والـremaining وreset والفترات المجدولة قريب من plugin المرجعي، مع اختبارات محلية جيدة للمسار السليم.

**الفروق المؤثرة:**

1. عولج استقلال `/key` و`/credits`: إذا تعطل أحدهما أو كان رده غير صالح، يحتفظ المحول ببيانات الآخر ويسجل تشخيص المصدر؛ وإذا لم ينجح أي مصدر يحتفظ بخطأ `/key` بدل تحويله إلى نجاح فارغ.
2. عولج سجل Activity وفق المصدر الحالي: يجلب history واليوم UTC المكتمل الأخير، يقصر النتائج إلى آخر 30 يومًا المكتمل، يزيل الصفوف المتطابقة ويرفض التعارضات، ويتحقق من التاريخ والعدادات والأرقام وحدود حجم الرد. يدخل `byok_usage_inference` في التكلفة الإجمالية مع بقائه منفصلًا في metadata، ولا تُضاف reasoning tokens مرة ثانية إلى token total.
3. عولج شرط النقل: عنوان API الافتراضي والمخصص يقبل HTTPS فقط؛ يرفض المحول HTTP قبل إرسال أي مفتاح. ويطابق حساب `limit_remaining` المصدر المرجعي بقصه إلى `[0, limit]` بدل الرجوع إلى usage أقدم عندما يتجاوز الحد أو يصبح سالبًا.
4. لم يُجر اختبار بحساب OpenRouter حي؛ نجاح المسارات الخارجية، ودلالات الخادم الفعلية، والتوافق التشغيلي ما زالت غير مثبتة خارج اختبارات النقل الاصطناعي.

**مراجع محلية:** [OpenRouter adapter](../../crates/core/src/providers/openrouter.rs)، [مصادر المفاتيح](../../crates/core/src/auth_sources.rs)، [عقود adapters](../../crates/core/tests/contracts.rs).  
**مرجع CodexBar:** [OpenRouter provider notes](https://github.com/steipete/CodexBar/blob/main/docs/openrouter.md)، [OpenRouter plugin implementation](https://github.com/steipete/CodexBar/blob/main/Sources/CodexBarCore/Resources/Plugins/openrouter.js)، [OpenRouter activity tests](https://github.com/steipete/CodexBar/blob/main/Tests/CodexBarTests/OpenRouterUsageStatsTests.swift).

## OpenCode Go

**المتوافق:** مسارات Console (`zen/go/v1/usage` و`console/api/...`) والـworkspace header، وmeters للـ5h/week/month، وتحويل micro-cent وfallbackات billing الأساسية ممثلة محليًا. أصبح `access.endsAt` موعد تجديد، ويملأ reset الشهر فقط عندما لا يرسل meter الشهر reset صالحًا؛ وتُقبل أسماء reset النسبية التي يستخدمها المرجع، ويُقرأ `usage.renewAt` قبل fallback إلى الجذر. التحويلات تُتبع حتى 10 مرات، فقط إلى HTTPS من الأصل نفسه، مع الحفاظ على دلالات 301/302/303 مقابل 307/308. هذه السياسة provider-specific ولا تغيّر النقل المشترك. استراتيجية المصدر account-scoped تتبع web/local/API، وSQLite المحلي معلّم كتقدير لا كقياس authoritative.

**الفروق المؤثرة:**

1. إثراء رصيد Console المحلي ما زال بعد استعلام الاستخدام بالتتابع؛ CodexBar يبدأ طلب Zen balance اختياريًا بالتوازي مع استعلام الاشتراك ويضع مهلة محدودة للانضمام، ثم يجعله fallback إلزاميًا في حالات `noSubscription` أو payload usage المفقود فقط. يلزم بعد ذلك نقل قواعد التوازي والاسترداد مع الحفاظ على تشخيص المصدر وحدود الحساب.
2. redirect المحلي أشد تقييدًا من المرجع عمدًا: يثبت scheme والـhost والـport معًا (HTTPS same-origin)، والمرجع يقارن الـhost ويشترط HTTPS. هذا يمنع إرسال Cookie/Bearer إلى منفذ مختلف، لكنه قد يرفض تحويلًا مشروعًا على منفذ بديل.
3. لم يُجر اختبار بحساب OpenCode Go حي؛ قواعد النقل والتحليل مثبتة باختبارات اصطناعية ومقارنة مصدرية فقط.

**مراجع محلية:** [OpenCode Go adapter](../../crates/core/src/providers/opencode_go.rs)، [مصدر usage المحلي](../../crates/core/src/providers/opencode_go_local.rs)، [HTTP transport](../../crates/core/src/transport.rs).  
**مرجع CodexBar:** [OpenCode provider notes](https://github.com/steipete/CodexBar/blob/main/docs/opencode.md)، [OpenCode Go fetcher](https://raw.githubusercontent.com/steipete/CodexBar/main/Sources/CodexBarCore/Providers/OpenCodeGo/OpenCodeGoUsageFetcher.swift)، [legacy fallback](https://raw.githubusercontent.com/steipete/CodexBar/main/Sources/CodexBarCore/Providers/OpenCodeGo/OpenCodeGoLegacyFallback.swift).

## المنسق المشترك

في Rust المحلي، Adaptive الافتراضي هو 2 دقيقة بعد تفاعل حديث، 5 دقائق حتى ساعة، 15 دقيقة حتى 4 ساعات، ثم 30 دقيقة؛ ويهبط إلى 30 دقيقة في low-power/thermal constrained. نشاط البرمجة الحديث يسرّع الخمول الطويل إلى 5 دقائق. هناك أيضًا refresh قريب من reset مع grace قدره 30 ثانية وحد أدنى 5 ثوانٍ. راجع [RefreshCoordinator](../../crates/core/src/refresh.rs).

CodexBar يوثق نطاقًا تكيفيًا مماثلًا 2–30 دقيقة، لكنه يجعل قراءة نشاط الوكلاء وضعًا منفصلًا اختياريًا يتطلب الموافقة، بينما السياسة المحلية تستخدم إشارة coding activity في Adaptive نفسه. إذًا الفارق ليس مدد polling وحدها؛ بل مصدر إشارة النشاط وحدود موافقة المستخدم. يجب إبقاء هذا فرقًا للمنسق، لا نسبته إلى adapters.

مرجع: [CodexBar refresh loop](https://github.com/steipete/CodexBar/blob/main/docs/refresh-loop.md).

## ترتيب العمل المقترح

1. **OpenCode Go:** استكمال إثراء الرصيد والـfallback المتوازي بعقود تغطي `noSubscription` وأخطاء الشبكة والتحليل؛ ثم اختبار الحساب الحي.
2. **Codex:** إعادة فحص قيمة Web Dashboard enrichment الاختياري مقابل WHAM فقط، مع الإبقاء على جلسة المتصفح المستوردة للحساب المختار ورفض مصادر `auth.json`/CLI؛ Codex هو الأولوية الأعلى للمشروع.
3. **Claude وOpenRouter:** التحقق الحي مع الحساب/المفتاح المخصص للاختبار عند توفرهما؛ الاختبارات الحالية اصطناعية.
4. **Antigravity:** لا يُضاف مصدر `agy` قبل العثور على دليل موثوق يربطه بهوية الحساب، مع استمرار حفظ عزل الحسابات.

هذه البنود تخص adapters التي لم تُحدّث بعد. قبل كل تغيير، تُراجع نسخة CodexBar من المصدر والاختبارات المقابلة لأن فرع `main` متغير، ثم تُكيّف الفكرة مع Windows وعزل الحسابات بدل نسخ تنفيذ macOS حرفيًا.

## التحقق وحدود المراجعة

- تحقق دفعة Codex: `cargo test -p codex-usage-core` نجح (97 اختبارًا وحدويًا و11 اختبار عقد)، ونجح `cargo fmt --all -- --check` و`git diff --check`. لم تُعَد اختبارات الحزم الأخرى في هذه الدفعة.
- تحقق دفعة Claude السابقة: `cargo test -p codex-usage-core` نجح (99 اختبارًا وحدويًا و15 اختبار عقد)، و`cargo test -p codex-usage-windows-auth` نجح (12 اختبارًا).
- تحقق استعادة Claude: `cargo test --workspace` نجح (99 core unit، و19 contract، و13 Windows-auth؛ إضافةً لاختبارات الأدوات والـdoc tests)، ونجح `cargo fmt --all -- --check` و`git diff --check`.
- بقية المزودين: مقارنة source/docs/tests فقط في هذه الجولة؛ لم يُشغّل تكامل حقيقي مع حسابات أو خدمات خارجية، ولم تُشغّل اختبارات Swift/CodexBar.
- CodexBar source snapshot reported by the OpenRouter audit: `173afc88a4171f013c53f8c5167f64808565f913`; the upstream `main` can change after this review date.
- لا تحتوي هذه المراجعة على بيانات اعتماد أو مفاتيح حسابات.
