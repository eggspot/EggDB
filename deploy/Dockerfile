FROM scratch
ARG EGGDB_BINARY=eggdb
COPY ${EGGDB_BINARY} /eggdb
USER 65532:65532
VOLUME ["/data"]
EXPOSE 5432 9187
ENTRYPOINT ["/eggdb"]
CMD ["serve", "/data/eggdb.eggdb", "--create", "--listen", "0.0.0.0:5432", "--tls-self-signed", "--metrics-listen", "0.0.0.0:9187", "--password-file", "/secrets/password"]
