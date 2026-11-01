SELECT table_name FROM user_tables ORDER BY table_name;
SELECT SYS_CONTEXT('USERENV', 'SESSION_USER'), SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM dual;
SELECT COUNT(*) FROM employees;
SELECT employee_id, first_name, last_name FROM employees WHERE department_id = 90 ORDER BY employee_id;
SELECT d.department_name, COUNT(*) FROM employees e JOIN departments d ON d.department_id = e.department_id GROUP BY d.department_name ORDER BY d.department_name;
SELECT e.last_name, m.last_name FROM employees e JOIN employees m ON e.manager_id = m.employee_id WHERE m.employee_id = 101 ORDER BY e.last_name;
SELECT TO_CHAR(hire_date, 'YYYY-MM-DD') FROM employees WHERE employee_id = 100;
SELECT last_name FROM employees ORDER BY employee_id FETCH FIRST 3 ROWS ONLY;
SELECT job_id, COUNT(*) FROM employees GROUP BY job_id HAVING COUNT(*) > 5 ORDER BY job_id;
SELECT NVL(TO_CHAR(department_id), 'none') FROM employees WHERE employee_id = 178;
SELECT COUNT(*) FROM emp_details_view;
SELECT constraint_name FROM user_constraints WHERE table_name = 'EMPLOYEES' AND constraint_type = 'R' ORDER BY constraint_name;
SELECT c.constraint_name, cc.column_name FROM user_constraints c JOIN user_cons_columns cc ON cc.constraint_name = c.constraint_name WHERE c.table_name = 'JOB_HISTORY' AND c.constraint_type = 'P' ORDER BY cc.position;
SELECT index_name FROM user_indexes WHERE table_name = 'EMPLOYEES' ORDER BY index_name;
SELECT sequence_name, increment_by FROM user_sequences ORDER BY sequence_name;
SELECT view_name FROM user_views;
SELECT column_name, data_type, nullable FROM user_tab_columns WHERE table_name = 'REGIONS' ORDER BY column_id;
SELECT value FROM nls_session_parameters WHERE parameter = 'NLS_DATE_FORMAT';
SELECT COUNT(*) FROM all_users WHERE username = 'HR';
SELECT 'regions' AS "Table", 5 AS "provided", count(1) AS "actual" FROM hr.regions
UNION ALL
SELECT 'countries' AS "Table", 25 AS "provided", count(1) AS "actual" FROM hr.countries
UNION ALL
SELECT 'departments' AS "Table", 27 AS "provided", count(1) AS "actual" FROM hr.departments
UNION ALL
SELECT 'locations' AS "Table", 23 AS "provided", count(1) AS "actual" FROM hr.locations
UNION ALL
SELECT 'employees' AS "Table", 107 AS "provided", count(1) AS "actual" FROM hr.employees
UNION ALL
SELECT 'jobs' AS "Table", 19 AS "provided", count(1) AS "actual" FROM hr.jobs
UNION ALL
SELECT 'job_history' AS "Table", 10 AS "provided", count(1) AS "actual" FROM hr.job_history;
SELECT object_name, object_type FROM user_objects WHERE object_type IN ('PROCEDURE', 'TRIGGER') ORDER BY object_type, object_name;
SELECT trigger_name, status FROM user_triggers ORDER BY trigger_name;
SELECT SUBSTR(text, 1, 25) FROM user_source WHERE name = 'ADD_JOB_HISTORY' AND line = 1;
