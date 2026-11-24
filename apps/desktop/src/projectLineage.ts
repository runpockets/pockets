/**
 * Pure lineage grouping for the pocket list: which drafts hang under which
 * installed pocket. Lives beside the wire shape it reads (the `Project`
 * record exported by CreationPanel.tsx) but stays dependency-free — it only
 * sees the structural subset below, so any consumer can group without
 * pulling in the panel.
 *
 * Honesty rule: a draft is a child only when its *persisted* `editedFrom`
 * names an installed record present in the same list. Nothing is ever
 * matched by name, manifest id, digest or "looks like a copy" — a lineage
 * native did not record does not exist here, and an unrecorded relationship
 * must never be drawn in the UI.
 */

/** The structural subset of CreationPanel's `Project` this grouping reads. */
export interface LineageRecord {
  id: string;
  phase: "draft" | "installed";
  /** Parent lineage stamped natively only on drafts imported as a copy of
   *  an installed/built pocket; absent (or null) otherwise. */
  editedFrom?: string | null;
}

/** One top-level entry in catalog order: the record itself and — for an
 *  installed parent — the lineage drafts rendered directly beneath it, in
 *  catalog order. Grouped drafts never appear as their own entry; every
 *  other entry has an empty `drafts`. */
export interface ProjectGroup<T extends LineageRecord> {
  project: T;
  drafts: T[];
}

/**
 * Groups a project list into renderable top-level entries. Only an
 * installed record *in the list* can be a parent: an orphan draft (lineage
 * id not in the list), a draft pointing at another draft, and a draft
 * without the field all stay top level, exactly as they render today. An
 * `editedFrom` on an installed record is ignored for grouping — native
 * never stamps one, and inventing a parent for a running pocket would be a
 * guess.
 */
export function groupByLineage<T extends LineageRecord>(
  projects: readonly T[],
): Array<ProjectGroup<T>> {
  const installedIds = new Set(
    projects
      .filter((project) => project.phase === "installed")
      .map((p) => p.id),
  );

  const childrenOf = new Map<string, T[]>();
  const grouped = new Set<string>();
  for (const project of projects) {
    const parentId = project.editedFrom;
    if (
      project.phase !== "draft" ||
      parentId === undefined ||
      parentId === null ||
      !installedIds.has(parentId)
    ) {
      continue;
    }
    const siblings = childrenOf.get(parentId);
    if (siblings) siblings.push(project);
    else childrenOf.set(parentId, [project]);
    grouped.add(project.id);
  }

  const groups: Array<ProjectGroup<T>> = [];
  for (const project of projects) {
    if (grouped.has(project.id)) continue;
    groups.push({ project, drafts: childrenOf.get(project.id) ?? [] });
  }
  return groups;
}
