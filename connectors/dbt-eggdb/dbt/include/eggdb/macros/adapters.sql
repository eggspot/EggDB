{% macro eggdb__create_table_as(temporary, relation, sql) -%}
  {{ return(postgres__create_table_as(temporary, relation, sql)) }}
{%- endmacro %}

{% macro eggdb__get_catalog(information_schema, schemas) -%}
  {{ return(postgres__get_catalog(information_schema, schemas)) }}
{%- endmacro %}

{% macro eggdb__list_relations_without_caching(schema_relation) -%}
  {% call statement('list_relations_without_caching', fetch_result=True) -%}
    select
      '{{ schema_relation.database }}' as database,
      table_name as name,
      table_schema as schema,
      case when table_type = 'VIEW' then 'view' else 'table' end as type
    from information_schema.tables
    where table_schema ilike '{{ schema_relation.schema }}'
  {%- endcall %}
  {{ return(load_result('list_relations_without_caching').table) }}
{%- endmacro %}

{% macro eggdb__current_timestamp() -%}
  now()
{%- endmacro %}
