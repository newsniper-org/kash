---
name: kash — async/await syntax는 POSIX 채택 전까지 영구 보류
description: kash에 async/await류 새 키워드/syntax 도입 제안 금지 — POSIX 최신 개정판에 정식 포함되기 전까지 구현 검토 자체 하지 않음
type: feedback
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash 설계에서 async/await 또는 그와 유사한 *구조적 동시성 키워드*는 **POSIX 최신 개정판에 정식 포함되기 전까지는 구현 검토조차 하지 않는다.**

**Why:** 사용자가 명시적으로 지정 ("async/await syntax는 'POSIX 최신 개정판에 포함되기 전까지는 구현 검토조차 안 함'"). 추론된 이유:
- POSIX `&` + `wait` + `wait -p var` 조합이 사실상 async/await의 셸 idiomatic form — 표현력 부족 아님
- 새 키워드 도입은 mental model overhead + parser 복잡도 + 기존 셸 호환성 파동
- POSIX 표준이 움직이지 않은 상태에서 invention하면 향후 POSIX이 다른 형태로 결정 시 호환 부담
- "복각이 아니다" 정신은 자유로운 invention 허용이지만, 이 영역에는 *명시적 abstention*

**How to apply:**
- 사용자 요청으로 동시성/병렬 처리 기능 논의가 나와도 `async`/`await`/`go`/`spawn` 같은 키워드 도입 제안 **하지 말 것**.
- 기존 셸 primitive (`&`, `wait`, `coproc`, `(cmd)`, process substitution) 의 조합으로 표현.
- 패턴 자체가 좋아질 필요가 있다고 판단되면 *first-party utility* (`parallel-run` 같은) 형태로 제안 — 키워드 아닌 명령.
- 이 정책은 향후 POSIX standard가 async/await 류 syntax를 채택하는 시점에 자동 해제. 그 시점에 사용자에게 확인 후 도입 검토.
- 이 원칙을 다른 invented syntax 일반에 적용하지 말 것 — 사용자는 capture lists, `mode` 키워드, `typeclass`, `namespace`, `use namespace` 등 invention은 환영. 특정적으로 async/await 류만 해당.
