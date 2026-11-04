-- Simplified TPC-C. Composite keys are encoded in one integer primary key:
--   district.id   = w * 10 + d                 (d in 0..9)
--   customer.id   = district.id * 3000 + c     (c in 0..2999)
--   stock.id      = w * 100000 + i             (i in 1..99999)
--   orders.id     = district.id * 1000000 + o_id
--   order_line.id = orders.id * 16 + line      (line in 1..5)

CREATE TABLE warehouse (
  id  NUMBER PRIMARY KEY,
  ytd NUMBER
);

CREATE TABLE district (
  id        NUMBER PRIMARY KEY,
  next_o_id NUMBER,
  ytd       NUMBER
);

CREATE TABLE customer (
  id          NUMBER PRIMARY KEY,
  balance     NUMBER,
  ytd_payment NUMBER,
  payment_cnt NUMBER
);

CREATE TABLE item (
  id    NUMBER PRIMARY KEY,
  price NUMBER
);

CREATE TABLE stock (
  id        NUMBER PRIMARY KEY,
  quantity  NUMBER,
  ytd       NUMBER,
  order_cnt NUMBER
);

CREATE TABLE orders (
  id     NUMBER PRIMARY KEY,
  c_id   NUMBER,
  ol_cnt NUMBER
);

CREATE TABLE order_line (
  id     NUMBER PRIMARY KEY,
  i_id   NUMBER,
  qty    NUMBER,
  amount NUMBER
);

-- Item id 0 = unused line. Item id -1 raises -20002 (the TPC-C 1% rollback).
CREATE OR REPLACE PROCEDURE new_order(
  p_w IN NUMBER, p_d IN NUMBER, p_c IN NUMBER,
  p_i1 IN NUMBER, p_q1 IN NUMBER,
  p_i2 IN NUMBER, p_q2 IN NUMBER,
  p_i3 IN NUMBER, p_q3 IN NUMBER,
  p_i4 IN NUMBER, p_q4 IN NUMBER,
  p_i5 IN NUMBER, p_q5 IN NUMBER
) IS
  v_d_key  NUMBER;
  v_o_id   NUMBER;
  v_o_key  NUMBER;
  v_ol_cnt NUMBER := 0;
  v_i      NUMBER;
  v_q      NUMBER;
  v_price  item.price%TYPE;
  v_s_qty  stock.quantity%TYPE;
BEGIN
  v_d_key := p_w * 10 + p_d;
  SELECT next_o_id INTO v_o_id FROM district WHERE id = v_d_key FOR UPDATE;
  UPDATE district SET next_o_id = next_o_id + 1 WHERE id = v_d_key;

  IF p_i1 <> 0 THEN v_ol_cnt := v_ol_cnt + 1; END IF;
  IF p_i2 <> 0 THEN v_ol_cnt := v_ol_cnt + 1; END IF;
  IF p_i3 <> 0 THEN v_ol_cnt := v_ol_cnt + 1; END IF;
  IF p_i4 <> 0 THEN v_ol_cnt := v_ol_cnt + 1; END IF;
  IF p_i5 <> 0 THEN v_ol_cnt := v_ol_cnt + 1; END IF;

  v_o_key := v_d_key * 1000000 + v_o_id;
  INSERT INTO orders (id, c_id, ol_cnt) VALUES (v_o_key, p_c, v_ol_cnt);

  FOR l IN 1 .. 5 LOOP
    IF l = 1 THEN v_i := p_i1; v_q := p_q1;
    ELSIF l = 2 THEN v_i := p_i2; v_q := p_q2;
    ELSIF l = 3 THEN v_i := p_i3; v_q := p_q3;
    ELSIF l = 4 THEN v_i := p_i4; v_q := p_q4;
    ELSE v_i := p_i5; v_q := p_q5;
    END IF;

    IF v_i <> 0 THEN
      IF v_i = -1 THEN
        RAISE_APPLICATION_ERROR(-20002, 'invalid item');
      END IF;
      SELECT price INTO v_price FROM item WHERE id = v_i;
      SELECT quantity INTO v_s_qty FROM stock WHERE id = p_w * 100000 + v_i FOR UPDATE;
      IF v_s_qty >= v_q + 10 THEN
        v_s_qty := v_s_qty - v_q;
      ELSE
        v_s_qty := v_s_qty - v_q + 91;
      END IF;
      UPDATE stock
         SET quantity = v_s_qty, ytd = ytd + v_q, order_cnt = order_cnt + 1
       WHERE id = p_w * 100000 + v_i;
      INSERT INTO order_line (id, i_id, qty, amount)
      VALUES (v_o_key * 16 + l, v_i, v_q, v_price * v_q);
    END IF;
  END LOOP;
END new_order;
/

CREATE OR REPLACE PROCEDURE payment(p_w IN NUMBER, p_d IN NUMBER, p_c IN NUMBER, p_amt IN NUMBER) IS
  v_d_key NUMBER;
BEGIN
  v_d_key := p_w * 10 + p_d;
  UPDATE warehouse SET ytd = ytd + p_amt WHERE id = p_w;
  UPDATE district SET ytd = ytd + p_amt WHERE id = v_d_key;
  UPDATE customer
     SET balance = balance - p_amt,
         ytd_payment = ytd_payment + p_amt,
         payment_cnt = payment_cnt + 1
   WHERE id = v_d_key * 3000 + p_c;
END payment;
/
