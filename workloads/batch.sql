-- Batch workload: monthly interest in committed blocks, with a progress table
-- written in the same block as the work (design doc section 7.3).
-- job_progress must hold a row (id = p_job, last_id = <id before the first account>).

CREATE TABLE account (
  id      NUMBER PRIMARY KEY,
  balance NUMBER,
  owner   VARCHAR2(40)
);

CREATE TABLE job_progress (
  id      NUMBER PRIMARY KEY,
  last_id NUMBER
);

CREATE OR REPLACE PROCEDURE monthly_interest(p_job IN NUMBER, p_batch IN NUMBER) IS
  v_last NUMBER;
  v_n    PLS_INTEGER;
BEGIN
  SELECT last_id INTO v_last FROM job_progress WHERE id = p_job;
  LOOP
    v_n := 0;
    FOR r IN (SELECT * FROM account WHERE id > v_last
              ORDER BY id FETCH FIRST p_batch ROWS ONLY) LOOP
      UPDATE account SET balance = balance + balance / 100 WHERE id = r.id;
      v_last := r.id;
      v_n    := v_n + 1;
    END LOOP;
    EXIT WHEN v_n = 0;
    UPDATE job_progress SET last_id = v_last WHERE id = p_job;  -- same block as the work
    COMMIT;                                                     -- one block = p_batch accounts
  END LOOP;
END monthly_interest;
/
