-- D6-14: searchable page description. Existing pages receive an empty value and
-- retain their previous generator contract until a new consolidate_v2 publish.
ALTER TABLE memory_pages ADD COLUMN description TEXT NOT NULL DEFAULT '';
ALTER TABLE page_revisions ADD COLUMN previous_description TEXT;
ALTER TABLE page_revisions ADD COLUMN new_description TEXT NOT NULL DEFAULT '';

-- FTS5 cannot add a virtual-table column in place. Recreate from current source
-- validity so stale/retired/expired/purge-pending pages do not re-enter search.
DROP TABLE page_fts;
CREATE VIRTUAL TABLE page_fts USING fts5(
  page_id UNINDEXED, title, description, body_md, tokenize = 'unicode61'
);
INSERT INTO page_fts (page_id,title,description,body_md)
SELECT p.id,p.title,p.description,p.body_md
FROM memory_pages p
WHERE p.status='published'
  AND EXISTS (SELECT 1 FROM page_sources ps
    WHERE ps.tenant_id=p.tenant_id AND ps.user_id=p.user_id AND ps.page_id=p.id)
  AND NOT EXISTS (SELECT 1 FROM page_sources ps
    JOIN memories m ON m.tenant_id=ps.tenant_id AND m.user_id=ps.user_id AND m.id=ps.memory_id
    WHERE ps.tenant_id=p.tenant_id AND ps.user_id=p.user_id AND ps.page_id=p.id
      AND (m.status<>'active' OR m.version<>ps.memory_version OR m.claim_sha256<>ps.claim_sha256
        OR (m.valid_until IS NOT NULL AND m.valid_until<=strftime('%Y-%m-%dT%H:%M:%fZ','now'))
        OR EXISTS (SELECT 1 FROM memory_retirements r
          WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)
        OR EXISTS (SELECT 1 FROM purge_jobs pj
          WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id AND pj.target_id=m.id
            AND pj.status IN ('pending','running'))));
