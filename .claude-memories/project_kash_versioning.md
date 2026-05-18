---
name: kash — versioning and compatibility policy (committed)
description: v<semver>-posix<POSIX 표준 버전> 형식, v(n)→v(n+2) 호환 보장, POSIX 개정은 별개 트랙
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash 버전 / 호환성 정책.

## 버전 표기

```
v<MAJOR>.<MINOR>.<PATCH>-posix<POSIX 표준 연도>
```

예:
- `v1.0.0-posix2017` — 초기 release, POSIX.1-2017 baseline
- `v1.3.5-posix2017` — patch/minor 업데이트
- `v2.0.0-posix2017` — major bump (within same POSIX track)
- `v1.0.0-posix2024` — POSIX.1-2024 baseline 별도 트랙

## 호환성 보장

**`vN-posixS` 사용자 코드는 `v(N+2)-posixS` 까지 작동 보장.**

- 즉 한 POSIX 트랙 안에서 *2 major 버전 deprecation 윈도*
- 예: `v1-posix2017` 으로 작성된 script는 `v2-posix2017`, `v3-posix2017` 까지 무수정 실행 가능
- `v4-posix2017` 부터는 v1 코드가 깨질 수 있음 (사전 deprecation 거친 후)
- Semver 정신 살리되, 셸 ecosystem의 긴 lifetime 감안 — 2 major window 제공

### Deprecation 절차 (잠정)

- `v(N+1)-posixS`: deprecation warning 출력 (사용자 인식)
- `v(N+2)-posixS`: 여전히 작동 (last guaranteed version)
- `v(N+3)-posixS`: 변경/제거 가능

## POSIX 개정 대응 — 별도 트랙

POSIX 표준 자체가 개정되면 (`POSIX.1-2024` 등) **별도 트랙으로 다룸**:
- `vM-posix2017` 과 `vN-posix2024` 는 별개 호환 라인
- 한 트랙의 호환 보장이 다른 트랙으로 자동 이전되지 않음
- 사용자는 명시적으로 트랙 선택 (shebang, pragma 등)

```sh
#!/usr/bin/env kash --posix-track=2024     # POSIX.1-2024 트랙
```

(정확한 트랙 selector 문법은 미결 — `--posix-track` vs `--posix=2024` 등 후속 결정)

## v1/v2/v2+ 용어 의미 (지금까지의 사용 컨텍스트)

지금까지 메모리/문서에서 `v1`/`v2+` 등으로 자주 언급한 항목들 (HTTP utility, async/await, generic types, multi-method dispatch 등)의 의미를 명확화:

- "v1" = 첫 ship 가능한 stable release (`v1.0.0-posix<현 시점 표준>`)
- "v2+" = 다음 major release 이상에서 검토
- "Defer to v2" = v1.x patch에서는 다루지 않음

## 미결

남은 항목 (process/impl):
- Deprecation warning 메시지 형식 — impl
- 호환 보장 위반 시 bug 분류 / fix 정책 — meta-process (별도)

기타 모두 project_kash_sweep_v1.md에서 해소 — selector는 `--posix-track=2024` + pragma `posix-track 2024`, 첫 release는 POSIX.1-2024, API stability tier는 v1 모든 기능 동일 보장 (experimental tier 도입 안 함).

**How to apply:** v1 lock된 기능 변경 시 호환 윈도 (`v(N+2)`) 의식. 새 기능 추가는 minor bump으로 가능. 기존 기능 의미 변경은 major bump 필요.
