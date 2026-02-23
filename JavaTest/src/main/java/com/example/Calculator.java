package com.example;

public class Calculator {

    public int add(int a, int b) {
        return a + b;
    }

    public int subtract(int a, int b) {
        return a - b;
    }

    public int multiply(int a, int b) {
        return a * b;
    }

    public int divide(int a, int b) {
        if (b == 0) {
            throw new ArithmeticException("Cannot divide by zero");
        }
        return a / b;
    }

    public boolean isPositive(int n) {
        return n > 0;
    }

    public int factorial(int n) {
        if (n < 0) {
            throw new IllegalArgumentException("Negative input");
        }
        if (n <= 1) {
            return 1;
        }
        return n * factorial(n - 1);
    }
}
