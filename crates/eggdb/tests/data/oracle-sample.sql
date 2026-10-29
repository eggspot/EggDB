REM EggDB smoke-test sample: a small SQL*Plus script in the shape SQL Developer and Oracle's
REM sample schema installers write (item 43). Frozen at the v0.2.0 tag (D96).
SET DEFINE OFF;
PROMPT Creating the bakery tables ...
CREATE TABLE bakers
    ( baker_id NUMBER(4) CONSTRAINT bakers_pk PRIMARY KEY
    , name     VARCHAR2(30) CONSTRAINT bakers_name_nn NOT NULL ENABLE
    ) SEGMENT CREATION IMMEDIATE PCTFREE 10 TABLESPACE "USERS";

CREATE TABLE loaves
    ( loaf_id  NUMBER(6)
    , baker_id NUMBER(4)
    , kind     VARCHAR2(20)
    , baked    DATE
    , CONSTRAINT loaves_baker_fk FOREIGN KEY (baker_id) REFERENCES bakers
    );

BEGIN
  INSERT INTO bakers VALUES (1, 'Ada & Co');
  INSERT INTO bakers VALUES (2, 'Grace');
END;
/

Insert into LOAVES (LOAF_ID,BAKER_ID,KIND,BAKED) values (10,1,'Rye',to_date('17-JUN-24','DD-MON-RR'));
Insert into LOAVES (LOAF_ID,BAKER_ID,KIND,BAKED) values (11,1,'Sourdough',to_date('18-JUN-24','DD-MON-RR'));
Insert into LOAVES (LOAF_ID,BAKER_ID,KIND,BAKED) values (12,2,'Rye',to_date('19-JUN-24','DD-MON-RR'));
Insert into LOAVES (LOAF_ID,BAKER_ID,KIND,BAKED) values (13,2,'Brioche',null);

CREATE OR REPLACE PROCEDURE rye_count (total OUT NUMBER) IS
BEGIN
  SELECT COUNT(*) INTO total FROM loaves WHERE kind = 'Rye';
END rye_count;
/
