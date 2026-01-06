---
name: high-level-architecture
description: When making code changes or reviewing existing code, check if architecture diagram matches implementation, and keep the diagram in sync.
---

# Generate High-level Architecture Diagram and Description

## Instructions
Using a single entity for each `mod`, create a Mermaid diagram to show the dependency as edges between entities. Include the Mermaid source code in its own block as well.

Below the chart, generate a one sentence description of each `mod` about its functionality. When modules are nested, use proper indentation to organize the entries. If there are more than 2 levels of indentation, create a subsction for further nested modules and reference it in the higher-level views.
