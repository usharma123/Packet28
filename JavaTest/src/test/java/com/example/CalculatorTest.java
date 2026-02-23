package com.example;

import static org.junit.Assert.*;
import org.junit.Test;

public class CalculatorTest {

    private final Calculator calc = new Calculator();

    @Test
    public void testAdd() {
        assertEquals(5, calc.add(2, 3));
        assertEquals(0, calc.add(-1, 1));
    }

    @Test
    public void testSubtract() {
        assertEquals(1, calc.subtract(3, 2));
    }

    @Test
    public void testMultiply() {
        assertEquals(6, calc.multiply(2, 3));
    }

    @Test
    public void testDivide() {
        assertEquals(2, calc.divide(6, 3));
    }

    @Test(expected = ArithmeticException.class)
    public void testDivideByZero() {
        calc.divide(1, 0);
    }

    @Test
    public void testIsPositive() {
        assertTrue(calc.isPositive(5));
        assertFalse(calc.isPositive(-1));
        assertFalse(calc.isPositive(0));
    }

    @Test
    public void testFactorial() {
        assertEquals(1, calc.factorial(0));
        assertEquals(1, calc.factorial(1));
        assertEquals(120, calc.factorial(5));
    }
}
