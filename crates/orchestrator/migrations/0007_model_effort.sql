-- Which Claude model and effort level an agent runs with by default, and a task's own choice
-- (the brain picks one per task, within the owner's allow-list). Null: the next level decides
-- (task, then agent, then the fleet's default); a node's own --model / --effort still wins.
alter table agents add column model text;
alter table agents add column effort text
    check (effort in ('low', 'medium', 'high', 'xhigh', 'max'));
alter table tasks add column model text;
alter table tasks add column effort text
    check (effort in ('low', 'medium', 'high', 'xhigh', 'max'));
