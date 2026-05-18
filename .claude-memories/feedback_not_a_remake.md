---
name: New shell is a redesign, not a ksh93 reproduction
description: When designing this shell, treat ksh93u+m as grammar inspiration only — don't hedge or ask about preserving ksh93's quirks, bugs, or questionable design choices
type: feedback
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
새 쉘 설계 논의에서, ksh93u+m의 *문법*만을 베이스로 채택했다는 점을 잊지 말 것. 의미론 차원에서 ksh93의 의도가 무엇이었든, 그것이 POSIX를 깨거나 부적절하면 그냥 새 쉘에서는 다르게 설계하면 된다. **호환 여부를 묻거나 hedging할 필요 없음.**

**Why:** 사용자가 두 번에 걸쳐 같은 취지로 짜증 섞인 교정("버그/특이점까지 재현할 필요는 없잖아;;;", "이건 복각이 아니야;;;")을 했다. 내가 ksh93의 subshell 최적화로 인한 관찰 가능한 부수효과 같은 "design 의도지만 POSIX 위반인" 사례들을 새 쉘에서 어떻게 처리할지 진지한 결정 사항인 양 다뤘기 때문. 사용자에게 이는 자명한 것이었음 — 새 설계이므로 그냥 더 나은 쪽(POSIX 준수)을 택하면 됨.

**How to apply:**
- ksh93의 문법은 차용, 의미론은 처음부터 다시 결정한다는 디폴트로 사고할 것.
- "ksh93u+m-compliant mode"는 *사용자가 호환 스크립트를 작성할 수 있도록 돕는 도구*이지, 새 쉘 내부 의미론을 ksh93에 묶는 장치가 아님. 이 모드에서도 새 쉘의 더 나은 의미론을 유지하면 됨.
- ksh93의 알려진 함정/bad practice들에 대해서는 "보존할지 묻기"가 아니라 "기본적으로 안 가져온다" 가정으로 진행. 정말 trade-off가 있는 경우에만 묻기.
- 이 원칙은 zsh, bash 차용분에도 동일하게 적용 — 기능을 가져온다고 해서 원본 쉘의 quirk까지 가져올 필요 없음.
