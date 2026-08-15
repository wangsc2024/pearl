You are synthesising collected facts into a short narrative summary for an operator.

Task: {{task_id}} (type: {{task_type}})

Facts collected mechanically, which you must not contradict or extend:

{{payload}}

Write the summary as JSON matching this shape exactly, and output nothing else:

{
  "summary": "two or three sentences describing what the facts show",
  "highlights": ["the most important point", "the next most important"],
  "concerns": ["anything the facts suggest needs attention"],
  "sources": ["the keys from the facts above that each claim rests on"]
}

Rules:

- Every claim in `summary` and `highlights` must be supported by the facts above. If the
  facts do not support a claim, leave it out.
- `sources` must reference keys that actually appear in the facts. Do not invent references.
- If the facts are empty or unusable, return `concerns` explaining why and leave `summary`
  as an empty string. Do not fill the gap with plausible content.
- Output JSON only. No prose before or after, no code fences.
