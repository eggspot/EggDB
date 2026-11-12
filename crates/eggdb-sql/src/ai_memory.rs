/// The statements `CREATE EXTENSION eggdb_ai` runs: a semantic cache of answers looked up by
/// embedding distance, and agent memory recalled by hybrid search (D205).
pub(crate) const AI_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_ai;
CREATE TABLE eggdb_ai.semantic_cache (id bigserial PRIMARY KEY, namespace text NOT NULL, prompt text NOT NULL, embedding text NOT NULL, answer text NOT NULL, created_at timestamptz NOT NULL DEFAULT now(), expires_at timestamptz, hits bigint NOT NULL DEFAULT 0);
CREATE INDEX semantic_cache_namespace ON eggdb_ai.semantic_cache (namespace);
CREATE TABLE eggdb_ai.memories (id bigserial PRIMARY KEY, agent text NOT NULL, content text NOT NULL, embedding text NOT NULL, metadata jsonb, created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX memories_agent ON eggdb_ai.memories (agent);
CREATE FUNCTION eggdb_ai.cache_put(namespace text, prompt text, answer text, embedding text DEFAULT NULL, ttl interval DEFAULT NULL) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    id bigint;
BEGIN
    INSERT INTO eggdb_ai.semantic_cache (namespace, prompt, embedding, answer, expires_at)
        VALUES (namespace, prompt, coalesce(embedding, eggdb_embed(prompt)), answer, CASE WHEN ttl IS NULL THEN NULL ELSE now() + ttl END)
        RETURNING semantic_cache.id INTO id;
    RETURN id;
END
$$;
CREATE FUNCTION eggdb_ai.cache_get(namespace text, prompt text, embedding text DEFAULT NULL, max_distance double precision DEFAULT 0.1) RETURNS TABLE (answer text, cached_prompt text, distance double precision) LANGUAGE plpgsql AS $$
DECLARE
    wanted text := coalesce(embedding, eggdb_embed(prompt));
    found_id bigint;
BEGIN
    SELECT c.id INTO found_id FROM eggdb_ai.semantic_cache c
        WHERE c.namespace = cache_get.namespace AND (c.expires_at IS NULL OR c.expires_at > now()) AND (c.embedding <=> wanted) <= max_distance
        ORDER BY c.embedding <=> wanted, c.id DESC LIMIT 1;
    IF found_id IS NULL THEN
        RETURN;
    END IF;
    UPDATE eggdb_ai.semantic_cache SET hits = hits + 1 WHERE id = found_id;
    RETURN QUERY SELECT c.answer, c.prompt, (c.embedding <=> wanted)::double precision FROM eggdb_ai.semantic_cache c WHERE c.id = found_id;
END
$$;
CREATE FUNCTION eggdb_ai.cache_evict(namespace text DEFAULT NULL) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    removed bigint;
BEGIN
    DELETE FROM eggdb_ai.semantic_cache c WHERE c.expires_at IS NOT NULL AND c.expires_at <= now() AND (cache_evict.namespace IS NULL OR c.namespace = cache_evict.namespace);
    GET DIAGNOSTICS removed = ROW_COUNT;
    RETURN removed;
END
$$;
CREATE FUNCTION eggdb_ai.remember(agent text, content text, embedding text DEFAULT NULL, metadata jsonb DEFAULT NULL) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    id bigint;
BEGIN
    INSERT INTO eggdb_ai.memories (agent, content, embedding, metadata)
        VALUES (agent, content, coalesce(embedding, eggdb_embed(content)), metadata)
        RETURNING memories.id INTO id;
    RETURN id;
END
$$;
CREATE FUNCTION eggdb_ai.recall(agent text, query text, k integer DEFAULT 5, embedding text DEFAULT NULL) RETURNS TABLE (id bigint, content text, score double precision, created_at timestamptz) LANGUAGE plpgsql AS $$
DECLARE
    wanted text := coalesce(embedding, eggdb_embed(query));
BEGIN
    RETURN QUERY
    WITH mine AS (
        SELECT m.id, m.content, m.embedding, m.created_at FROM eggdb_ai.memories m WHERE m.agent = recall.agent
    ), semantic AS (
        SELECT mine.id, row_number() OVER (ORDER BY mine.embedding <=> wanted, mine.id DESC) AS rank FROM mine
    ), lexical AS (
        SELECT mine.id, row_number() OVER (ORDER BY ts_rank(to_tsvector('english', mine.content), plainto_tsquery('english', query)) DESC, mine.id DESC) AS rank
        FROM mine WHERE to_tsvector('english', mine.content) @@ plainto_tsquery('english', query)
    )
    SELECT mine.id, mine.content, (rrf_score(semantic.rank) + coalesce(rrf_score(lexical.rank), 0))::double precision AS score, mine.created_at
    FROM mine JOIN semantic ON semantic.id = mine.id LEFT JOIN lexical ON lexical.id = mine.id
    ORDER BY 3 DESC, mine.id DESC LIMIT greatest(k, 0);
END
$$;
CREATE FUNCTION eggdb_ai.forget(agent text, memory_id bigint) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM eggdb_ai.memories m WHERE m.agent = forget.agent AND m.id = memory_id;
    RETURN FOUND;
END
$$;
"#;
