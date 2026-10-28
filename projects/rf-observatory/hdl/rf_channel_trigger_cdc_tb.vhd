-- Exercise the trigger diagnostic using the same vendor synchronizer and
-- framer connection as the board integration patch. Check digital latency,
-- sticky diagnostic behavior and reset into a new clean epoch. The complete
-- board top-level wiring is separately reviewed; this simulation cannot model
-- metastability or establish physical CDC/timing behavior.
library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use std.env.all;

entity rf_channel_trigger_cdc_tb is end entity;
architecture simulation of rf_channel_trigger_cdc_tb is
    signal clock : std_logic := '0';
    signal reset : std_logic := '1';
    signal source_arm, arm_rx, input_valid, output_valid : std_logic := '0';
    signal i, q : signed(15 downto 0);
    signal seen_clean, seen_error : boolean := false;
    signal latest_error : std_logic := '0';
begin
    clock <= not clock after 5 ns;
    sync : entity work.synchronizer
        generic map (RESET_LEVEL => '0')
        port map (reset => reset, clock => clock, async => source_arm, sync => arm_rx);
    framer : entity work.rf_channel_framer
        port map (clock => clock, reset => reset, s_valid => input_valid,
            s_i => to_signed(1500,16), s_q => to_signed(-400,16), source_rx2 => '0',
            configuration_error => arm_rx, fifo_overflow => '0',
            m_valid => output_valid, m_ready => '1', m_i => i, m_q => q);
    monitor : process(clock)
        variable slot : natural range 0 to 7 := 0;
    begin
        if rising_edge(clock) then
            if reset = '1' then
                slot := 0; seen_clean <= false; seen_error <= false;
                latest_error <= '0';
            elsif output_valid = '1' then
                if slot = 0 then
                    assert i = signed'(x"5246") and q = signed'(x"4f42")
                        report "framing misaligned" severity failure;
                elsif slot = 1 then
                    latest_error <= i(6);
                    if i(6) = '1' then seen_error <= true;
                    else seen_clean <= true; end if;
                end if;
                slot := (slot + 1) mod 8;
            end if;
        end if;
    end process;
    stimulus : process
        procedure cycles(n : positive) is
        begin
            for count in 1 to n loop wait until falling_edge(clock); end loop;
        end procedure;
    begin
        cycles(3); reset <= '0'; input_valid <= '1';
        cycles(100);
        assert seen_clean and not seen_error report "clean epoch falsely marked" severity failure;
        wait for 2 ns; source_arm <= '1';
        wait for 1 ns;
        assert arm_rx = '0' report "diagnostic bypassed synchronizer" severity failure;
        cycles(4);
        assert arm_rx = '1' report "armed diagnostic failed to cross" severity failure;
        cycles(100);
        assert seen_error report "synchronized arm not latched into record" severity failure;
        wait for 2 ns; source_arm <= '0';
        cycles(4);
        assert arm_rx = '0' report "disarm failed to cross" severity failure;
        cycles(50);
        assert latest_error = '1'
            report "configuration diagnostic did not remain asserted after disarm" severity failure;
        reset <= '1'; input_valid <= '0'; cycles(3);
        reset <= '0'; input_valid <= '1'; cycles(100);
        assert seen_clean and not seen_error and latest_error = '0'
            report "reset did not begin clean epoch" severity failure;
        report "synchronized trigger diagnostic contract passed" severity note;
        finish;
    end process;
end architecture;
