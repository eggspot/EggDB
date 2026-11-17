from dbt.adapters.base import AdapterPlugin

from dbt.adapters.eggdb.connections import EggdbConnectionManager, EggdbCredentials
from dbt.adapters.eggdb.impl import EggdbAdapter
from dbt.include import eggdb

Plugin = AdapterPlugin(
    adapter=EggdbAdapter,
    credentials=EggdbCredentials,
    include_path=eggdb.PACKAGE_PATH,
    dependencies=["postgres"],
)

__all__ = ["EggdbAdapter", "EggdbConnectionManager", "EggdbCredentials", "Plugin"]
