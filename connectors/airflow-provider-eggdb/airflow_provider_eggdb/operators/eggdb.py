from airflow.models import BaseOperator
from airflow.providers.common.sql.operators.sql import SQLExecuteQueryOperator

from airflow_provider_eggdb.hooks.eggdb import EggDBHook


class EggDBOperator(SQLExecuteQueryOperator):
    template_fields = ("sql", "parameters")

    def __init__(self, *, eggdb_conn_id="eggdb_default", dialect=None, **kwargs):
        super().__init__(conn_id=eggdb_conn_id, **kwargs)
        self.eggdb_conn_id = eggdb_conn_id
        self.dialect = dialect

    def get_db_hook(self):
        return EggDBHook(eggdb_conn_id=self.eggdb_conn_id, dialect=self.dialect)


class EggDBBackupOperator(BaseOperator):
    template_fields = ("path",)

    def __init__(self, *, path, clone=False, eggdb_conn_id="eggdb_default", **kwargs):
        super().__init__(**kwargs)
        self.path = path
        self.clone = clone
        self.eggdb_conn_id = eggdb_conn_id

    def execute(self, context):
        EggDBHook(eggdb_conn_id=self.eggdb_conn_id).backup(self.path, clone=self.clone)
        return self.path
