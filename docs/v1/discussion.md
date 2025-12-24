# v1 Discussion

## Highlights

### Command Abstraction: Unified vs Protocol-Specific

The most substantive design discussion centered on how to structure command types across protocols.

**Options explored:**
- **Option A:** Unified `Command` enum with all fields from all protocols
- **Option B:** Shared trait with protocol-specific implementations
- **Option C:** Completely separate types per protocol (vertical slices)

User raised a key concern about Option A: "If two protocols have quite [different] optionality associated with the same basic command, option A can introduce a lot of options and only a subset is relevant for each, without a good way for readers to tell which ones are for what protocol."

When asked about scaling to 5 protocols with 3-200 commands each, the analysis shifted to maintainability over time. The deciding factor: who writes and maintains the code?

From the AI's perspective: "I can jump into any module instantly... I don't have the human 'pain' of switching contexts." Vertical slices mean adding a new protocol doesn't require understanding (or risking breakage of) existing protocols.

**Final choice:** Option C (vertical slices) for isolation and simplicity.

### Parser Implementation: Crate vs Hand-Rolled

User asked about the `redis-protocol` crate's memory handling before deciding. Research showed it offers three frame types with different allocation strategies (BytesFrame, OwnedFrame, RangeFrame).

Decision: Hand-rolled parser to match existing memcached parser style and avoid adding dependencies. RESP's length-prefixed format is simpler than memcached's text protocol anyway.

### RESP Version Negotiation

Chose RESP2 as default with RESP3 opt-in via HELLO command. This maximizes client compatibility—older clients work out of the box, newer clients can upgrade.

## Questions Raised

- Should we add integration tests using actual Redis client libraries (redis-py, jedis)?
- What additional commands are needed for real-world cache usage?
- Should there be a way to run both protocols simultaneously on different ports?

---

## Full Transcript

<details>
<summary>Complete conversation log</summary>

[Session conducted via Claude Code CLI. Key exchanges summarized in Highlights above. Full transcript not captured in this milestone.]

</details>
