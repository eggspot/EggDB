FROM python:3.12-slim
WORKDIR /airbyte/integration_code
RUN pip install --no-cache-dir psycopg2-binary==2.9.9
COPY airbyte_eggdb ./airbyte_eggdb
ARG CONNECTOR=source
ENV CONNECTOR=${CONNECTOR}
ENTRYPOINT ["sh", "-c", "exec python -m airbyte_eggdb.${CONNECTOR} \"$@\"", "--"]
LABEL io.airbyte.version=0.1.0
