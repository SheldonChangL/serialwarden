# 設計：從 GUI 觸發燒錄（Flash Profiles）

狀態：草案，待審（2026-10-06）
範圍：daemon、web API、web UI。MCP 不在 v1 範圍內。

## 1. 要解決的問題

燒錄目前只能用 `serialwarden run -- <flasher>`，而且必須在 daemon 那台機器的 shell 上執行。實際的工作流程是：板子接在 jetubuntu，人在 Mac 上用 SSH tunnel 開 GUI 看 log。想燒錄的時候，得另開 terminal、`ssh` 過去、記得工具路徑和參數，還要把 `$SERIALWARDEN_LEASE_PATH` 用單引號包好，才能在遠端展開。

目標是在 GUI 上按一下按鈕，用事先定義好的方式燒錄指定的 firmware，並且在同一個畫面看到工具輸出、lease 的空檔，以及燒完後的開機 log。

## 2. 核心約束：GUI 只能「選」，不能「寫指令」

web UI 沒有任何認證。凡是連得到 `127.0.0.1:5590` 的對象都能呼叫 API，包括：

- 本機瀏覽器
- 持有 SSH tunnel 的人
- **同一台機器上的其他使用者帳號**（loopback 不分使用者）

一旦能從 GUI 執行燒錄，這個 API 就等於能以 daemon 的使用者身分在主機上跑程式。所以 v1 的設計原則是：

1. **要執行什麼程式，只能由手寫的設定檔決定。** GUI 和 API 能做的，只有從設定檔已定義的 profile 中挑一個，再從該 profile 允許的目錄中挑一個 firmware 檔。不接受任何自由輸入的指令或參數。
2. **用 argv 執行，不經過 shell。** 參數裡的佔位符只會整個替換成一個 argv 元素，不做字串拼接，因此不存在 shell 注入。
3. **預設關閉。** 沒有設定檔就沒有這個功能：不顯示按鈕，API 回 404。
4. **設定只能手改。** API、GUI、MCP 都不能修改設定，這跟 `rules.toml` 的 danger 清單是同一個原則。

這個設計**防不住**同一台主機上的其他本機使用者：他們本來就能透過現有的 write API 對 serial port 寫任意 bytes，現在則多了「用既有 profile 燒錄允許目錄內的檔案」的能力。所以文件要寫明：只在單人使用的主機上啟用。真正的解法是 v2 的 token 認證，見第 10 節。

## 3. 設定檔：`flash.toml`

放在 `rules.toml` 所在的設定目錄（Linux：`~/.config/serialwarden/flash.toml`；macOS：`~/Library/Application Support/serialwarden/flash.toml`）。

```toml
# 以 KC002 在 jetubuntu 上的實際用法為例
[[profile]]
name = "kc002-uart"                       # GUI 顯示的名稱，同一檔案內唯一
devices = ["usb-067b_23a3_EOBUb147612"]   # 只在這些 device 上出現；省略＝所有 device
argv = [
  "/home/user/bin/uartfwburn.linux",
  "-p", "{port}",
  "-f", "{firmware}",
  "-b", "3000000", "-U", "-r",
]
workdir = "{firmware_dir}"                # 選用；預設是 firmware 所在的目錄
timeout_s = 600                           # 必填；逾時就 kill，並收回 port
firmware_dir = "/home/user/new_space/projects/fw/KC002/formal"
firmware_glob = "*/flash_ntz*.bin"        # 相對於 firmware_dir
checksum_file = "md5sum.txt"              # 選用；與 firmware 同目錄的 md5sum/sha256sum 格式檔
env = { }                                 # 選用；額外的環境變數
```

**佔位符**只有三個，每一個都必須單獨佔滿一個 argv 元素，例如不能寫成 `"--port={port}"`：

| 佔位符 | 替換成 |
|---|---|
| `{port}` | lease 交出來的 device path，也就是 `SERIALWARDEN_LEASE_PATH` |
| `{firmware}` | 使用者選定的 firmware 經正規化後的絕對路徑 |
| `{firmware_dir}` | 該 firmware 所在的目錄 |

**載入與驗證**

- 每次列出 profile 時重新讀取檔案，改完設定不用重啟 daemon。
- 檔案有錯時，整個功能停用，並在 GUI 顯示錯誤原因。不會「部分可用」。

驗證規則：

- `argv[0]` 必須是絕對路徑，指向存在且可執行的檔案。
- `timeout_s` 必須大於 0。
- `firmware_dir` 必須是絕對路徑，而且存在。
- 佔位符只能是上表的三個，而且必須單獨佔滿一個 argv 元素。
- 檔案擁有者必須是 daemon 的使用者，且 group 與 others 都不可寫入，否則拒絕載入。這跟 ssh 對 `authorized_keys` 的檢查同一個道理：設定檔決定了要執行什麼程式。

## 4. Firmware 的選取與驗證

- **列出候選檔**：用 `firmware_glob` 在 `firmware_dir` 底下找符合的檔案。每個候選檔都先 canonicalize，結果必須仍在 `firmware_dir` 之內；透過 symlink 跳到目錄外的檔案一律排除。
- **顯示資訊**：每個檔案顯示相對路徑、大小、修改時間、SHA-256（只計算一次，依 mtime+size 快取）。有 `checksum_file` 時，同時顯示比對結果：「相符 / 不符 / 檔案裡沒有這一項」。
- **防止檔案被換掉**：發起燒錄時，client 要把使用者確認畫面上看到的 SHA-256 一起送出。daemon 重新計算，對不上就回 409。這樣可以避免在「確認」到「執行」之間，檔案被換掉卻沒人發現。
- **checksum 不符時**：GUI 預設不允許送出，要另外勾選「仍要燒錄」才行。這次覆寫會記進事件裡。

v1 不支援從瀏覽器上傳 firmware。上傳任意檔案再拿去執行，攻擊面又大了一級；而 firmware 放到主機上，本來就可以用既有的 `scp` 或 build 流程處理。

## 5. 執行流程（daemon 端的 job runner）

新增 `serialwardend::flash` 模組，裡面的 job runner 依序執行以下步驟，每個步驟都會寫入事件流：

1. **驗證**：profile 存在且適用於這個 device，firmware 合法，SHA-256 相符，`argv[0]` 可執行。任一項失敗就直接回錯誤，**不會**取得 lease。
2. **取得 lease**：呼叫 `PortConfigApi::acquire_lease`，跟 `serialwarden run` 用的是同一條路。`timeout_s` 傳 profile 的值，`command` 欄位記錄的是**渲染後的 argv**。這一步會產生既有的 `lease_start` 事件。
3. **spawn 子程序**：stdin 接 `/dev/null`，stdout 與 stderr 用 pipe 接回來，cwd 設為 `workdir`，環境變數多帶 `SERIALWARDEN_LEASE_PATH`，子程序放進自己的 process group。
4. **轉送輸出**：子程序輸出逐行寫進 `<data_dir>/jobs/<job_id>.log`，同時推送給 WS 的訂閱者。很多燒錄工具用 `\r` 更新進度列，所以要沿用 daemon 既有的 CR/LF 斷行邏輯。
5. **逾時處理**：daemon 自己計時。時間到先對整個 process group 送 SIGTERM，10 秒後還沒結束就送 SIGKILL。daemon 端原本就有的 lease deadline（`reclaim_expired_leases`）也照樣生效，成為第二道防線。
6. **收尾**：子程序結束後呼叫 `release_lease(token, exit_code)`，產生既有的 `lease_end` 事件，port 重新開啟、繼續錄製，燒完的開機 log 會自然接上。
7. **job 狀態持久化**：狀態寫在 `<data_dir>/jobs/<job_id>.json`，依序為 `pending → running → succeeded | failed | timed_out | cancelled | interrupted`。

**daemon 在燒錄途中重啟**：
- 不能只靠 service manager 清掉子程序。systemd 預設的 `KillMode=control-group` 會收掉 cgroup 內所有後代程序，但 launchd 只收 daemon 自己的 process group，而子程序在第 3 步已經放進獨立的 process group，launchd 收不到它。所以要加兩道防線：
  - daemon 收到 SIGTERM 時，先終止所有執行中的 job（流程同第 5 步的逾時處理），再結束自己。
  - job 的 `.json` 記錄子程序的 pid 和啟動時間。重啟後，如果同一個 pid 還活著，而且啟動時間相符（避免誤殺 pid 被重用後的其他程序），就先把它結束。
- 之後，既有的 residual-lease 回收機制會把 port 拿回來。
- 狀態還停在 `running` 的 job 會被標成 `interrupted`，GUI 會明確顯示「燒錄被中斷，板子狀態未知」。

**同時燒錄**：lease 本身就是互斥的，所以同一個 device 同時只能有一個 job。第二個請求回 409，並附上目前持有者是誰。不同 device 之間可以並行。

## 6. 事件與稽核

這次新增兩種事件，其他沿用既有的 `lease_start` / `lease_end`：

| 事件 | 欄位 |
|---|---|
| `flash_requested` | `job_id`、`profile`、`firmware`（相對路徑）、`sha256`、`checksum_status`、`override`（是否覆寫了 checksum 不符）、`requested_by: "gui"`、`client`（Host/Origin 摘要） |
| `flash_finished` | `job_id`、`state`、`exit_code`、`duration_ms`、`log`（job log 的路徑）、`output_tail`（最後 20 行，方便在 timeline 上直接看到錯誤） |

這樣一來，在 audit 視圖用 `--context <seq>` 就能一次看到：誰在什麼時候燒了哪個檔、工具最後說了什麼、燒完板子印了什麼。

## 7. API

所有會改變狀態的 POST 都要帶 `X-SerialWarden-Intent: flash` header。帶自訂 header 的請求不屬於 CORS simple request，瀏覽器會先發 preflight；daemon 不回任何 CORS header，preflight 就會失敗。這樣一來，即使 Origin 檢查哪天出現漏洞，跨站網頁仍然送不出燒錄請求。

| 方法與路徑 | 用途 |
|---|---|
| `GET /api/devices/{id}/flash/profiles` | 列出適用的 profile 與 firmware 候選檔（含大小、mtime、sha256、checksum 狀態）。設定檔有錯時回傳錯誤原因 |
| `POST /api/devices/{id}/flash` | body 為 `{profile, firmware, sha256, override_checksum}`，成功回 `202 {job_id}`；busy 或 sha 不符回 409 |
| `GET /api/jobs/{job_id}` | job 狀態與 log 尾端 |
| `POST /api/jobs/{job_id}/cancel` | 取消執行中的 job，送 SIGTERM，10 秒後 SIGKILL，最後狀態為 `cancelled` |
| WS `/api/stream` | 新增 `job_output` 與 `job_state` 兩種訊息 |

## 8. GUI

- **入口**：device 標題列、port 設定旁邊加一個「Flash…」按鈕。只有在這個 device 有適用的 profile 時才出現。
- **燒錄對話框**：
  - 選 profile，接著選 firmware。列表顯示相對路徑、大小、時間、短 SHA-256、checksum 狀態，預設依時間由新到舊排序。
  - 唯讀顯示**實際會執行的 argv**，用真實路徑渲染，讓使用者確認自己按下去的是什麼。
  - 顯示 timeout 秒數。
  - 確認按鈕寫成「Flash `<device>` with `<file>`」，**不是預設焦點**。這跟 approval card「Allow once 不能是預設焦點」是同一條規則：避免一按 Enter 就送出。
- **執行中**：
  - 一個 output 面板即時顯示工具輸出，附經過時間和 Cancel 按鈕。
  - 主 log 照常顯示：timeline 上標出 lease 空檔，`lease_end` 之後立刻接上開機 log。
- **結束後**：顯示狀態與 exit code，並附連結跳到 `lease_end` 之後的第一行開機 log。
- **錯誤處理**：
  - 設定檔有錯：按鈕仍會出現，但打開後顯示具體錯誤，例如「`argv[0]` 不可執行：…」，不會靜默消失。
  - checksum 不符：送出按鈕停用，要勾選覆寫才能送。

## 9. 不做的事（v1）

- **MCP / agent 燒錄**：燒錄的後果大致不可逆，agent 要做這件事必須先經過 gate，而且應該無條件要求人工核准（等同 danger pattern）。這要等 v1 在真機上驗證過，再另外設計。
- **CLI `serialwarden flash <profile> <firmware>`**：重用同一套 job API，對 ssh 一行指令很方便，但可以放到 phase 2。
- **瀏覽器上傳 firmware**、在 GUI 上編輯 profile、自由輸入參數。
- **多使用者主機的權限隔離**，留給 v2 的認證。

## 10. 之後（v2）

先加 web 認證。做法是 daemon 啟動時產生一個 token，寫進權限 0600 的檔案；GUI 第一次開啟時需要用這個 token 交換 cookie。有了認證之後，才適合加入瀏覽器上傳 firmware、MCP gated flash，以及多人共用主機的支援。

## 11. 測試計畫

- **單元測試**：
  - `flash.toml` 解析與驗證：佔位符位置、檔案權限、`argv[0]` 檢查。
  - argv 渲染。
  - firmware 列舉：glob、symlink 逃逸、`..`。
  - checksum 檔解析（md5sum 與 sha256sum 兩種格式）。
- **整合測試**：用 mock-device PTY 搭配一個假燒錄腳本，涵蓋下列情境：
  - 正常流程：腳本確認能開啟 `{port}`、印出含 `\r` 的進度、exit 0。事件順序應為 `flash_requested → lease_start → lease_end → flash_finished`，而且 port 之後恢復錄製。
  - 非 0 exit code。
  - 逾時：子程序被 kill，狀態為 `timed_out`。
  - Cancel。
  - 執行前 firmware 被替換：回 409，不產生 lease。
  - 同一 device 第二個請求：回 409。
  - daemon 重啟：job 標為 `interrupted`，lease 被回收。
- **安全測試**：
  - 跨站 POST、沒有 intent header 的 POST，都回 403 或 404。
  - 沒有設定檔時，所有 flash API 都回 404。
- **E2E（Playwright）**：
  - 對話框：argv 預覽、確認按鈕不是預設焦點、checksum 不符時送出按鈕停用。
  - output 面板。
  - timeline 上的 lease 空檔。
- **真機驗收**（列入 `docs/manual-checklist.md`）：在 Mac 透過 tunnel 開 GUI，對 jetubuntu 上的 KC002 用 `uartfwburn` 完整燒錄一次，確認開機 log 出現 `[KC002] FW ...`。

## 12. 工作量估計與分期

| 階段 | 內容 | 估計 |
|---|---|---|
| P1 | `flash.toml` 載入驗證、firmware 列舉、job runner、事件、API、單元與整合測試 | 1.5–2 天 |
| P2 | GUI 對話框、output 面板、WS 訊息、E2E | 1–1.5 天 |
| P3 | 文件（README、wiki Security-model、manual checklist）、真機驗收 | 0.5 天 |
| 之後 | CLI `serialwarden flash` | 0.5 天 |

## 13. 需要你決定的事

1. **工具輸出放哪裡**：要像本設計一樣，只存在 job log 與 `flash_finished.output_tail`，還是也寫進 device 的主 log？寫進去會讓 log 混入非 device 的資料，違反「log 就是 device 說的話」這個原則，所以我建議不寫。
2. **checksum 不符時**：允許勾選覆寫（本設計的做法），還是直接禁止？
3. **單人主機限制**：只寫進文件就好，還是要讓 daemon 偵測到多個登入使用者時拒絕啟用？我建議只寫進文件。偵測做法不可靠，很容易誤擋。
4. **CLI `serialwarden flash` 的時程**：要不要拉進 P1？它能讓 ssh 一行指令變成 `ssh jetubuntu serialwarden flash kc002-uart 20261001/flash_ntz.nn.bin`。
