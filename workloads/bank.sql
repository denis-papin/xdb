-- Bank workload: transfers between accounts, deposits and range audits.

CREATE TABLE account (
  id      NUMBER PRIMARY KEY,
  balance NUMBER CHECK (balance >= 0),
  owner   VARCHAR2(40)
);

CREATE TABLE ledger (
  id  NUMBER PRIMARY KEY,
  src NUMBER,
  dst NUMBER,
  amt NUMBER
);

CREATE SEQUENCE ledger_seq START WITH 1;

CREATE OR REPLACE PROCEDURE transfer(p_from IN NUMBER, p_to IN NUMBER, p_amt IN NUMBER) IS
  v_bal NUMBER;
BEGIN
  SELECT balance INTO v_bal FROM account WHERE id = p_from;
  IF v_bal < p_amt THEN
    RAISE_APPLICATION_ERROR(-20001, 'insufficient funds');
  END IF;
  UPDATE account SET balance = balance - p_amt WHERE id = p_from;
  UPDATE account SET balance = balance + p_amt WHERE id = p_to;
  INSERT INTO ledger VALUES (ledger_seq.NEXTVAL, p_from, p_to, p_amt);
END transfer;
/

CREATE OR REPLACE PROCEDURE deposit(p_id IN NUMBER, p_amt IN NUMBER) IS
BEGIN
  UPDATE account SET balance = balance + p_amt WHERE id = p_id;
END deposit;
/

CREATE OR REPLACE PROCEDURE audit_total(p_lo IN NUMBER, p_hi IN NUMBER) IS
  v NUMBER;
BEGIN
  SELECT SUM(balance) INTO v FROM account WHERE id BETWEEN p_lo AND p_hi;
  DBMS_OUTPUT.PUT_LINE(v);
END audit_total;
/

CREATE OR REPLACE PROCEDURE get_balance(p_id IN NUMBER) IS
  v NUMBER;
BEGIN
  SELECT balance INTO v FROM account WHERE id = p_id;
  DBMS_OUTPUT.PUT_LINE(v);
END get_balance;
/

-- Escrow-style transfer (O'Neil): no read of the balance. The CHECK constraint
-- refuses an overdraft (ORA-02290, code -2290); with escrow on, the shard accepts
-- the debit only if balance >= 0 holds in the worst case of all pending debits.
-- Difference with transfer: an unknown p_from is ignored (0 rows) instead of
-- raising NO_DATA_FOUND.
CREATE OR REPLACE PROCEDURE transfer_escrow(p_from IN NUMBER, p_to IN NUMBER, p_amt IN NUMBER) IS
BEGIN
  UPDATE account SET balance = balance - p_amt WHERE id = p_from;
  UPDATE account SET balance = balance + p_amt WHERE id = p_to;
  INSERT INTO ledger VALUES (ledger_seq.NEXTVAL, p_from, p_to, p_amt);
END transfer_escrow;
/
