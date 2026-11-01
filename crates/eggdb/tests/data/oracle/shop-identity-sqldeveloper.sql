--------------------------------------------------------
--  File created - Friday-October-30-2026   
--------------------------------------------------------
--------------------------------------------------------
--  DDL for Table ORDERS
--------------------------------------------------------

  CREATE TABLE "SHOP"."ORDERS" 
   (	"ORDER_ID" NUMBER(9,0), 
	"PLACED" DATE
   ) SEGMENT CREATION IMMEDIATE 
  PCTFREE 10 PCTUSED 40 INITRANS 1 MAXTRANS 255 
 NOCOMPRESS LOGGING
  TABLESPACE "USERS" ;
--------------------------------------------------------
--  DDL for Table TICKETS
--------------------------------------------------------

  CREATE TABLE "SHOP"."TICKETS" 
   (	"TICKET_ID" NUMBER GENERATED ALWAYS AS IDENTITY MINVALUE 1 MAXVALUE 9999999999999999999999999999 INCREMENT BY 1 START WITH 41 CACHE 20 NOORDER  NOCYCLE  NOKEEP  NOSCALE  NOT NULL ENABLE, 
	"CODE" VARCHAR2(10 BYTE) CONSTRAINT "TICKETS_CODE_CK" CHECK (code = UPPER(code)) DISABLE, 
	"ORDER_ID" NUMBER(9,0) CONSTRAINT "TICKETS_ORDER_FK" REFERENCES "SHOP"."ORDERS" ("ORDER_ID") DISABLE, 
	 CONSTRAINT "TICKETS_PK" PRIMARY KEY ("TICKET_ID")
  USING INDEX PCTFREE 10 INITRANS 2 MAXTRANS 255 COMPUTE STATISTICS 
  TABLESPACE "USERS"  ENABLE
   ) SEGMENT CREATION IMMEDIATE 
  PCTFREE 10 PCTUSED 40 INITRANS 1 MAXTRANS 255 
 NOCOMPRESS LOGGING
  TABLESPACE "USERS" ;
REM INSERTING into SHOP.ORDERS
SET DEFINE OFF;
Insert into SHOP.ORDERS (ORDER_ID,PLACED) values ('1',to_date('2026-10-01 09:30:00','YYYY-MM-DD HH24:MI:SS'));
Insert into SHOP.ORDERS (ORDER_ID,PLACED) values ('2',to_date('2026-10-02 14:00:00','YYYY-MM-DD HH24:MI:SS'));
REM INSERTING into SHOP.TICKETS
SET DEFINE OFF;
Insert into SHOP.TICKETS (TICKET_ID,CODE,ORDER_ID) values ('39','a1','1');
Insert into SHOP.TICKETS (TICKET_ID,CODE,ORDER_ID) values ('40','B2','2');
--------------------------------------------------------
--  DDL for Index ORDERS_ID_IX
--------------------------------------------------------

  CREATE UNIQUE INDEX "SHOP"."ORDERS_ID_IX" ON "SHOP"."ORDERS" ("ORDER_ID") 
  PCTFREE 10 INITRANS 2 MAXTRANS 255 COMPUTE STATISTICS 
  TABLESPACE "USERS" ;
--------------------------------------------------------
--  Constraints for Table ORDERS
--------------------------------------------------------

  ALTER TABLE "SHOP"."ORDERS" ADD CONSTRAINT "ORDERS_PK" PRIMARY KEY ("ORDER_ID")
  USING INDEX "SHOP"."ORDERS_ID_IX"  ENABLE;
--------------------------------------------------------
--  Ref Constraints for Table TICKETS
--------------------------------------------------------

  ALTER TABLE "SHOP"."TICKETS" ENABLE CONSTRAINT "TICKETS_ORDER_FK";
