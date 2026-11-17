def get_provider_info():
    return {
        "package-name": "airflow-provider-eggdb",
        "name": "EggDB",
        "description": "Hooks and operators for EggDB, over its PostgreSQL protocol.",
        "connection-types": [
            {
                "connection-type": "eggdb",
                "hook-class-name": "airflow_provider_eggdb.hooks.eggdb.EggDBHook",
            }
        ],
        "versions": ["0.1.0"],
    }
