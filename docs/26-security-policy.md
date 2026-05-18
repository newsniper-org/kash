# Shellshock-Class Vulnerability Prevention Policy

kash의 보안 baseline. Shellshock 부류 (CVE-2014-6271 등 6종) 취약점 원천 봉쇄.

## 영향 CVE

| CVE | 본질 |
|---|---|
| CVE-2014-6271 | env var의 `() { ... }` 패턴을 함수 정의로 파싱 + 닫는 `}` 뒤 명령도 실행 |
| CVE-2014-7169 | 6271 patch 미흡 — parser 혼란으로 파일 생성 |
| CVE-2014-6277 | 함수 parser uninitialized memory read |
| CVE-2014-6278 | 함수 parsing이 명령 실행으로 직결 |
| CVE-2014-7186 | heredoc parser stack overflow |
| CVE-2014-7187 | nested loop parser OOB read |

## 5원칙

### (P1) 환경변수는 절대 코드로 해석되지 않음
- bash `export -f`, env var의 `() { ... }` 자동 import — **kash 일절 지원 안 함**
- 환경변수는 항상 문자열 data
- 6271/6277/6278 봉쇄
- Transpiler가 bash `export -f` 만나면 error/무시

### (P2) Parser 메모리 안전 (Rust)
- Rust 구현이 자동으로 7186/7187 class 봉쇄
- 모든 parser 입력 bounds check, no UAF
- Parser depth 제한 (e.g., heredoc nesting 64)

### (P3) 외부 데이터의 묵시적 eval 금지
- 네트워크 응답, CGI env, IPC, 파일 내용 — *항상 data*
- 사용자 명시적 `eval`/`source` 없이 절대 code 실행 안 됨
- `-secure` modifier에서 `eval` 차단 (P3 강화)

### (P4) Function 정의는 source code에서만
- env, file content, network 응답 등 외부 string에서 함수 자동 생성 경로 없음

### (P5) TLS/검증 default ON, 우회는 명시적 + secure 거부
- HTTPS 검증 default ON
- `--insecure` 명시적 + stderr warning
- `-secure` 하에서는 우회 옵션 자체 거부

## `-secure` modifier 강화

기존 lock 항목에 **`eval` builtin 차단 추가** ([15-set-options.md](15-set-options.md) 참조).

## CGI / 서버 컨텍스트

- CGI env var (`QUERY_STRING`, `HTTP_*` 등) — bash에서는 Shellshock 진입점, kash는 P1으로 원천 봉쇄
- 권장 startup mode: `mode posix-strict-secure` 또는 `mode default-secure`

## 모든 first-party utility 적용

- Network utility: 응답 데이터 항상 data
- JSON utility: parser 안전성 (P2)
- HTTP utility (v2+): TLS default ON, `--insecure` `-secure` 거부

## 미결 (향후 검토)

- "Untrusted scope" 개념 / mode modifier
- Resource limit 기본값
- File `source` untrusted path 거부 옵션
- Sandbox 모드 (`-sandboxed`?)
