---
name: kash — Shellshock-class vulnerability prevention policy (committed)
description: 환경변수 비-함수화 + Rust 메모리 안전 + 외부 데이터 묵시적 eval 금지 + TLS default ON 등 시스템 차원 보안 정책
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash의 보안 baseline. Shellshock (CVE-2014-6271 등 6종) 부류 취약점 *원천 봉쇄* 위한 5원칙.

## 영향 CVE

| CVE | 본질 |
|---|---|
| CVE-2014-6271 | env var의 `() { ... }` 패턴을 함수 정의로 파싱 + 닫는 `}` 뒤 명령도 실행 |
| CVE-2014-7169 | 6271 patch 미흡 — parser 혼란으로 파일 생성 가능 |
| CVE-2014-6277 | 함수 parser uninitialized memory read (info leak) |
| CVE-2014-6278 | 함수 parsing이 명령 실행으로 직결 |
| CVE-2014-7186 | heredoc parser stack overflow |
| CVE-2014-7187 | nested loop parser OOB read |

## 5원칙 (모두 적용)

### (P1) 환경변수는 절대 코드로 해석되지 않음

- bash의 "exported function" (`export -f`, env var의 `() { ... }` 자동 import) 기능을 **kash는 일절 지원 안 함**.
- 환경변수는 항상 *문자열 data*. shell parsing context 아님.
- **6271/6277/6278 봉쇄**.
- Transpiler가 bash `export -f` 만나면 **error 또는 명시적 무시** (절대 환경변수 통한 함수 전파 안 함).

### (P2) Parser는 메모리 안전 (Rust)

- Rust 구현 결정 (project_kash_implementation.md)이 자동으로 **7186/7187 class 봉쇄**.
- 모든 parser 입력 (heredoc, nested loop, glob, JSONC 등)에서 bounds check, no UAF.
- Parser depth 제한 명시 (e.g., max heredoc nesting 64).

### (P3) 외부 데이터의 묵시적 eval 금지

- 네트워크 응답 (HTTP body/header), CGI env vars, IPC 메시지, 파일 내용 등 — *항상 data*.
- 사용자가 *명시적으로 `eval` 또는 `source`* 하지 않는 한 절대 code로 실행 안 됨.
- `eval` 사용은 사용자 책임 — `-secure` modifier에서 `eval` 자체 차단 (P3 강화).

### (P4) Function 정의는 source code에서만

- 함수 정의 (`function f { ... }`, `f() { }`) 는 *kash가 직접 파싱하는 script 텍스트*에서만 생성.
- env, file content, network 응답 등 외부 string에서 함수 자동 생성 경로 *없음*.

### (P5) TLS/검증은 default ON, 우회는 명시적 + secure에서 거부

- HTTPS 검증 default ON (HTTP utility 설계에 반영, v2+에서 lock).
- `--insecure` 등 우회 옵션은 명시적 + stderr warning.
- `-secure` modifier 하에서는 우회 옵션 자체를 거부 (modifier monotonicity로 강제).

## `-secure` modifier 강화 (committed)

set options 메모리 (project_shell_set_options.md) 의 `-secure` lock 항목에 **`eval` builtin 차단 추가** (P3과 보완).

```
-secure 강제 lock 항목 (업데이트):
  - 기존: errexit, pipefail, nounset, noclobber, null-glob → fail, (e) re-eval 금지, backticks 금지, warn-* 4종 강제 on, error-leaky-jobs 강제 on
  - 신규 추가: eval builtin 차단 (P3)
```

## CGI / 서버 컨텍스트 패턴

kash가 untrusted source에서 env var 받는 환경 (CGI 핸들러 등) 에서:

- `QUERY_STRING`, `HTTP_*` 등 표준 CGI env var는 그대로 data로 들어옴
- bash에서는 함수 정의로 파싱되어 Shellshock 발생 → kash는 (P1)으로 원천 봉쇄
- 권장 패턴: CGI startup에서 `mode posix-strict-secure` 또는 `mode default-secure` — eval/dynamic source 등 차단

## 모든 first-party utility에 적용

- Network utility (`tcp-connect` 등): 응답 데이터 항상 data, eval 없음
- JSON utility: JSONC 파싱 시 코드 실행 경로 없음 (P2)
- HTTP utility (v2+ 예정): 응답 body/header 항상 data, TLS default ON, `--insecure` `-secure` 거부

## 추가 검토 (미결, 향후 결정)

- "Untrusted scope" 개념 도입 가치 — CGI 등 명시적 untrusted 컨텍스트를 mode modifier로 marking 가능?
- Resource limit (max recursion depth, max heredoc nesting, max process count) 의 기본값
- File 읽기에서 trust boundary 명시 (예: `source` 가 untrusted path 시 거부 옵션)
- Sandbox 모드 (`-sandboxed`?) — fork/exec 제한, FS access 제한 등 (v2+)

**How to apply:** 보안 관련 후속 설계 (sandboxing, capability system, syscall restriction 등) 시 위 5원칙을 baseline. 새 기능이 P1-P5 중 하나라도 위반하는 듯하면 *반드시* 사용자에게 보고하고 설계 재검토.
